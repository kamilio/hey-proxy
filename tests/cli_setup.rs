use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

fn cli(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hey-proxy"));
    cmd.env("HOME", home)
        .env("CODEX_HOME", home.join("codex"))
        .stdin(Stdio::null());
    cmd
}

#[test]
fn init_is_minimal_private_and_preserves_existing_proxy_and_codex_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let codex = home.join("codex");
    std::fs::create_dir(&codex).unwrap();
    let sentinel = b"model = 'my-existing-model'\n";
    std::fs::write(codex.join("config.toml"), sentinel).unwrap();
    assert!(cli(home).arg("--help").output().unwrap().status.success());
    assert!(!home.join(".hey-proxy").exists());
    assert!(cli(home).arg("--init").output().unwrap().status.success());
    let path = home.join(".hey-proxy/config.json");
    let generated: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("../examples/minimal.config.json")).unwrap();
    assert_eq!(generated, expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let edited = b"{\"listen\":\"127.0.0.1:9090\",\"providers\":{\"openai\":{\"api_keys\":{\"default\":\"synthetic\"}}}}\n";
    std::fs::write(&path, edited).unwrap();
    assert!(cli(home).arg("--init").output().unwrap().status.success());
    assert_eq!(std::fs::read(path).unwrap(), edited);
    assert_eq!(std::fs::read(codex.join("config.toml")).unwrap(), sentinel);
    assert_eq!(std::fs::read_dir(codex).unwrap().count(), 1);
}

#[tokio::test]
async fn first_start_creates_only_proxy_config_and_missing_credentials_return_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = cli(dir.path())
        .args(["--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    struct Stop(std::process::Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let _stop = Stop(child);
    let first = lines.next().unwrap().unwrap();
    let url = first.strip_prefix("hey-proxy listening on ").unwrap();
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&json!({"model":"gpt-4.1","input":"test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(
        response.json::<Value>().await.unwrap()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No OpenAI credential configured")
    );
    assert!(
        reqwest::get(format!("{url}/logs/api?local=true"))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(!dir.path().join("codex").exists());
    assert!(!dir.path().join(".codex").exists());
    assert!(dir.path().join(".hey-proxy/config.json").exists());
}

#[test]
fn all_user_examples_validate_without_resolving_credentials_or_changing_files() {
    let dir = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")).unwrap()
    {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let temporary = dir.path().join(path.file_name().unwrap());
        std::fs::write(&temporary, &bytes).unwrap();
        let result = cli(dir.path())
            .arg("--config")
            .arg(&temporary)
            .arg("--init")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "Invalid example {}: {}",
            path.display(),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(std::fs::read(temporary).unwrap(), bytes);
    }
    assert!(!dir.path().join("codex").exists());
}
