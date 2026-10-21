use super::*;
use axum::{Json, Router, extract::State, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) fn fixture(path: &Path, expires_at: u64) {
    save(
        path,
        &Tokens {
            access_token: "codex-oat-synthetic-old".into(),
            refresh_token: "codex-refresh-old".into(),
            expires_at,
            account_id: Some("acct_synthetic_123".into()),
        },
    )
    .unwrap();
}

#[test]
fn encrypted_store_and_jwt_account_metadata_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codex.json");
    fixture(&path, now() + 3600);
    let content = fs::read_to_string(&path).unwrap();
    assert!(!content.contains("synthetic"));
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.refresh_token, "codex-refresh-old");
    assert_eq!(loaded.account_id.as_deref(), Some("acct_synthetic_123"));

    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "exp": 1890000000u64,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "org_workspace_42"
            }
        }))
        .unwrap(),
    );
    let jwt = format!("{header}.{payload}.sig");
    let (exp, account_id) = jwt_metadata(&jwt);
    assert_eq!(exp, Some(1890000000));
    assert_eq!(account_id.as_deref(), Some("org_workspace_42"));
}

#[test]
fn oauth_url_includes_codex_pkce_parameters() {
    let url = authorization_url(
        DEFAULT_ISSUER,
        "http://localhost:1455/auth/callback",
        "expected-state",
        "test-verifier",
    )
    .unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["client_id"], CLIENT_ID);
    assert_eq!(params["state"], "expected-state");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(b"test-verifier"))
    );
    assert_eq!(params["codex_cli_simplified_flow"], "true");
    assert_eq!(params["id_token_add_organizations"], "true");
    assert!(!url.as_str().contains("test-verifier"));
}

#[tokio::test]
async fn concurrent_refresh_rotates_once_and_preserves_account_id() {
    let count = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/oauth/token",
            post(
                |State(count): State<Arc<AtomicUsize>>, Json(body): Json<Value>| async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(body["refresh_token"], "codex-refresh-old");
                    assert_eq!(body["grant_type"], "refresh_token");
                    assert_eq!(body["client_id"], CLIENT_ID);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Json(json!({
                        "access_token": "codex-oat-synthetic-new",
                        "refresh_token": "codex-refresh-new",
                        "expires_in": 3600
                    }))
                },
            ),
        )
        .with_state(count.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("codex.json");
    fixture(&path, 1);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut jobs = Vec::new();
    for _ in 0..6 {
        let (path, url, client) = (path.clone(), url.clone(), client.clone());
        jobs.push(tokio::spawn(async move {
            TokenManager::default()
                .credentials(&path, &client, &url)
                .await
                .unwrap()
        }));
    }
    for job in jobs {
        let creds = job.await.unwrap();
        assert_eq!(creds.access_token, "codex-oat-synthetic-new");
        assert_eq!(creds.account_id.as_deref(), Some("acct_synthetic_123"));
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(load(&path).unwrap().refresh_token, "codex-refresh-new");
    server.abort();
}
