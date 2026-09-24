use crate::{
    access,
    config::{ClientConnection, Config, Mode},
    proxy, rollout,
};
use axum::{Router, extract::Request, response::IntoResponse};
use serde_json::{Value, json};
use std::path::Path;

async fn start(
    mut config: Config,
    access_config: Option<&Path>,
) -> (String, Config, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.listen = listener.local_addr().unwrap();
    let url = format!("http://{}", config.listen);
    let app = proxy::router_with(
        config.clone(),
        proxy::Options {
            access_config: access_config.map(Path::to_path_buf),
            ..Default::default()
        },
    )
    .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, config, task)
}
#[test]
fn legacy_configs_default_to_standalone_and_keys_are_persistent_private_and_distinct() {
    let mut value = serde_json::to_value(Config::test_fixture()).unwrap();
    value.as_object_mut().unwrap().remove("mode");
    assert_eq!(
        serde_json::from_value::<Config>(value).unwrap().mode,
        Mode::Standalone
    );
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.json");
    let first = access::ensure(&config_path, &["a".into(), "b".into()]).unwrap();
    let again = access::ensure(&config_path, &["a".into(), "c".into()]).unwrap();
    assert_eq!(first.local, again.local);
    assert_eq!(first.clients["a"], again.clients["a"]);
    assert_ne!(first.local, first.clients["a"]);
    assert_ne!(first.clients["a"], first.clients["b"]);
    assert!(again.accepts(&again.clients["c"]));
    assert!(!again.accepts("wrong"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(access::key_path(&config_path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn inventory_requires_valid_host_client_relationships_and_minimal_client_config() {
    let mut value = serde_json::to_value(Config::test_fixture()).unwrap();
    value["ssh_hosts"] = json!([
        {"host":"client-a","mode":"client","via":"shared"},
        {"host":"shared","mode":"host","url":"http://shared.local:8080","listen":"0.0.0.0:8080"}
    ]);
    serde_json::from_value::<Config>(value.clone())
        .unwrap()
        .validate()
        .unwrap();
    value["ssh_hosts"][0]["via"] = json!("missing");
    assert!(
        serde_json::from_value::<Config>(value)
            .unwrap()
            .validate()
            .is_err()
    );
    let mut client: Config = serde_json::from_value(json!({"mode":"client","listen":"127.0.0.1:8080","connection":{"url":"http://shared.local:8080","api_key":"access-key"}})).unwrap();
    client.validate().unwrap();
    client
        .api_keys
        .insert("upstream-secret".into(), "sk-secret".into());
    assert!(client.validate().is_err());
}
#[tokio::test]
async fn host_client_forwarding_authentication_and_connection_verification() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let upstream = Router::new().fallback(|request: Request| async move {
        if request.uri().path() == "/v1/models" {
            return axum::Json(json!({"data":[{"id":"gpt-4.1-mini"}]})).into_response();
        }
        assert_eq!(request.headers()["authorization"], "Bearer something");
        let bytes = axum::body::to_bytes(request.into_body(), 10000)
            .await
            .unwrap();
        axum::Json(serde_json::from_slice::<Value>(&bytes).unwrap()).into_response()
    });
    let upstream_task = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("host.json");
    let keys = access::ensure(&key_path, &["client-a".into()]).unwrap();
    let (host_url, host, host_task) = start(
        Config {
            mode: Mode::Host,
            upstream_url,
            ..Config::test_fixture()
        },
        Some(&key_path),
    )
    .await;
    let client = reqwest::Client::new();
    for path in [
        "/v1/models",
        "/v1/responses",
        "/logs/api",
        "/logs/dashboard.js",
    ] {
        assert_eq!(
            client
                .get(format!("{host_url}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{host_url}{path}"))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    let logged_in = client
        .post(format!("{host_url}/logs/login"))
        .form(&[("api_key", keys.local.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(logged_in.status(), 401); // Default client follows redirect without storing the cookie.
    let cookie_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let login = cookie_client
        .post(format!("{host_url}/logs/login"))
        .form(&[("api_key", keys.local.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(
        client
            .get(format!("{host_url}/logs/api"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let relay: Config = serde_json::from_value(json!({"mode":"client","listen":"127.0.0.1:8080","connection":{"url":host_url,"api_key":keys.clients["client-a"]}})).unwrap();
    let (relay_url, relay, relay_task) = start(relay, None).await;
    let response: Value = client
        .post(format!("{relay_url}/v1/responses"))
        .json(&json!({"model":"gpt-4.1","input":"hello"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["model"], "gpt-4.1-mini");
    assert_eq!(response["reasoning"]["effort"], "low");
    let logs: Value = client
        .get(format!("{host_url}/logs/api"))
        .bearer_auth(&keys.clients["client-a"])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(logs["entries"][0]["requested_model"], "gpt-4.1");
    assert_eq!(logs["entries"][0]["routed_model"], "gpt-4.1-mini");
    assert!(!logs.to_string().contains(&keys.local));
    let host_home = dir.path().join("host-codex");
    rollout::configure_codex_authenticated(
        &format!("{host_url}/v1"),
        Some(&host_home),
        None,
        Some(&keys.local),
    )
    .unwrap();
    rollout::verify(&host, &key_path, Some(&host_home), true)
        .await
        .unwrap();
    let relay_home = dir.path().join("client-codex");
    rollout::configure_codex_authenticated(
        &format!("{relay_url}/v1"),
        Some(&relay_home),
        None,
        None,
    )
    .unwrap();
    rollout::verify(
        &relay,
        &dir.path().join("client.json"),
        Some(&relay_home),
        true,
    )
    .await
    .unwrap();
    let mut bad = relay.clone();
    bad.connection = Some(ClientConnection {
        url: host_url,
        api_key: "wrong".into(),
    });
    let (_, bad, bad_task) = start(bad, None).await;
    assert!(
        rollout::verify(&bad, &dir.path().join("bad.json"), Some(&relay_home), true)
            .await
            .is_err()
    );
    host_task.abort();
    relay_task.abort();
    bad_task.abort();
    upstream_task.abort();
}

#[tokio::test]
#[allow(clippy::result_large_err)] // tungstenite handshake callback requires the unboxed HTTP error
async fn client_websocket_uses_host_access_key_and_host_routes_upstream() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (stream, _) = upstream.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
             response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.headers()["authorization"], "Bearer something");
                Ok(response)
            },
        )
        .await
        .unwrap();
        let message = socket.next().await.unwrap().unwrap();
        let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert_eq!(value["model"], "gpt-4.1-mini");
        socket.send(message).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host.json");
    let keys = access::ensure(&path, &["client".into()]).unwrap();
    let (host_url, _, host_task) = start(
        Config {
            mode: Mode::Host,
            upstream_url,
            ..Config::test_fixture()
        },
        Some(&path),
    )
    .await;
    let rejected =
        connect_async(format!("{}/v1/responses", host_url.replace("http:", "ws:"))).await;
    assert!(
        matches!(rejected, Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == 401)
    );
    let config = Config {
        mode: Mode::Client,
        api_keys: Default::default(),
        aliases: vec![],
        connection: Some(ClientConnection {
            url: host_url,
            api_key: keys.clients["client"].clone(),
        }),
        ..Default::default()
    };
    let (url, _, client_task) = start(config, None).await;
    let (mut socket, _) = connect_async(format!("{}/v1/responses", url.replace("http:", "ws:")))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"type":"response.create","model":"gpt-4.1"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(message.to_text().unwrap()).unwrap()["model"],
        "gpt-4.1-mini"
    );
    task.await.unwrap();
    host_task.abort();
    client_task.abort();
}

#[tokio::test]
async fn changing_host_mode_requires_restart_and_keeps_authentication_active() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config {
        mode: Mode::Host,
        listen: listener.local_addr().unwrap(),
        ..Default::default()
    };
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let keys = access::ensure(&path, &[]).unwrap();
    let app = proxy::router_with(
        config.clone(),
        proxy::Options {
            access_config: Some(path.clone()),
            source: Some((path.clone(), crate::config::fingerprint(&path))),
            ..Default::default()
        },
    )
    .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("http://{}/logs/api", config.listen);
    config.mode = Mode::Standalone;
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let client = reqwest::Client::new();
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth(keys.local)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    task.abort();
}
