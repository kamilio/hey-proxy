use hey_proxy::credentials::{CredentialResolver, validate_source};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn literal_and_shell_values_are_sensitive_and_validated() {
    let resolver = CredentialResolver::default();
    assert!(
        resolver
            .resolve("synthetic", Duration::from_secs(60))
            .await
            .unwrap()
            .is_sensitive()
    );
    assert_eq!(
        resolver
            .resolve("sh://printf 'synthetic\n'", Duration::from_secs(60))
            .await
            .unwrap(),
        "synthetic"
    );
    for bad in ["", "\n", "sh://", "sh://\0", "unknown://future/item/field"] {
        assert!(validate_source(bad).is_err());
    }
    for bad in [
        "sh://exit 1",
        "sh://printf ''",
        "sh://printf 'a\nb'",
        "sh://head -c 17000 /dev/zero",
    ] {
        assert!(
            resolver
                .resolve(bad, Duration::from_secs(60))
                .await
                .is_err()
        );
    }
}
#[tokio::test]
async fn concurrent_commands_are_single_flight_and_cache_expires() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("calls");
    let source = format!("sh://printf x >> '{}'; printf synthetic", marker.display());
    let resolver = Arc::new(CredentialResolver::default());
    let mut tasks = Vec::new();
    for _ in 0..30 {
        let resolver = resolver.clone();
        let source = source.clone();
        tasks.push(tokio::spawn(async move {
            resolver
                .resolve(&source, Duration::from_secs(60))
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), "synthetic");
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    resolver.resolve(&source, Duration::ZERO).await.unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "xx");
}
#[tokio::test]
async fn shell_failure_is_redacted_and_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("calls");
    let source = format!(
        "sh://printf x >> '{}'; printf SECRET >&2; exit 1",
        marker.display()
    );
    let resolver = CredentialResolver::default();
    for _ in 0..2 {
        let error = resolver
            .resolve(&source, Duration::from_secs(60))
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("SECRET"));
    }
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "xx");
}
#[tokio::test]
async fn sources_are_independent_and_config_edits_take_effect() {
    let resolver = CredentialResolver::default();
    assert_eq!(
        resolver
            .resolve("sh://printf first", Duration::from_secs(60))
            .await
            .unwrap(),
        "first"
    );
    assert_eq!(
        resolver
            .resolve("sh://printf second", Duration::from_secs(60))
            .await
            .unwrap(),
        "second"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_source_kills_descendants_and_releases_single_flight() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let leaked = dir.path().join("leaked");
    let source = format!(
        "sh://if test -f '{}'; then printf recovered; else (sleep 0.4; printf leaked > '{}') & printf ready > '{}'; wait; printf synthetic; fi",
        ready.display(),
        leaked.display(),
        ready.display()
    );
    let resolver = Arc::new(CredentialResolver::default());
    let task = tokio::spawn({
        let resolver = resolver.clone();
        let source = source.clone();
        async move { resolver.resolve(&source, Duration::from_secs(60)).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        resolver
            .resolve(&source, Duration::from_secs(60))
            .await
            .unwrap(),
        "recovered"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!leaked.exists(), "credential helper survived cancellation");
}

#[test]
fn onepassword_references_validate_without_becoming_literal_headers() {
    for source in [
        "op://Agents/hey-proxy/codex",
        "op://Vault With Spaces/Item/section/credential",
        "op://6vm5bepg2thg44tfve3aackkwe/item-id/credential",
    ] {
        validate_source(source).unwrap();
    }
    for source in [
        "op://",
        "op://vault",
        "op://vault/item",
        "op://vault//field",
        "op://vault/item/section/field/extra",
        "op://vault/item/../field",
        "op://vault/item/field\n",
        "op://vault/item/field?attribute=otp",
        "op://vault/item/field#fragment",
    ] {
        assert!(validate_source(source).is_err(), "{source:?}");
    }
}

#[cfg(unix)]
fn mock_op(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("op");
    std::fs::write(&cli, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    (dir, cli)
}

#[cfg(unix)]
#[tokio::test]
async fn onepassword_is_direct_single_flight_and_cached_only_in_memory() {
    let (dir, cli) = mock_op(
        r#"
test "$#" = 4 || exit 2
test "$1" = read || exit 3
test "$2" = --no-newline || exit 4
test "$3" = -- || exit 5
printf x >> "$0.calls"
printf '%s' "$4" > "$0.reference"
printf synthetic-op-key
"#,
    );
    let resolver = Arc::new(CredentialResolver::with_op_cli(&cli));
    let mut jobs = Vec::new();
    // Shell metacharacters remain a single opaque CLI argument.
    let reference = "op://Agents/item/$(touch SHOULD_NOT_EXIST)";
    for _ in 0..32 {
        let resolver = resolver.clone();
        jobs.push(tokio::spawn(async move {
            resolver
                .resolve(reference, Duration::from_secs(60))
                .await
                .unwrap()
        }));
    }
    for job in jobs {
        let value = job.await.unwrap();
        assert_eq!(value, "synthetic-op-key");
        assert!(value.is_sensitive());
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("op.calls")).unwrap(),
        "x"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("op.reference")).unwrap(),
        reference
    );
    resolver.resolve(reference, Duration::ZERO).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("op.calls")).unwrap(),
        "xx"
    );
    assert!(!std::path::Path::new("SHOULD_NOT_EXIST").exists());
    // A new resolver must ask the CLI again; no persistent plaintext cache exists.
    CredentialResolver::with_op_cli(cli)
        .resolve(reference, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("op.calls")).unwrap(),
        "xxx"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn onepassword_errors_are_redacted_and_never_fall_back_to_reference_as_key() {
    let (dir, cli) = mock_op(
        r#"printf x >> "$0.calls"; printf PRIVATE_OUTPUT; printf PRIVATE_ERROR >&2; exit 1"#,
    );
    let resolver = CredentialResolver::with_op_cli(cli);
    for _ in 0..2 {
        let message = resolver
            .resolve("op://Agents/item/credential", Duration::from_secs(60))
            .await
            .unwrap_err()
            .to_string();
        assert!(!message.contains("PRIVATE"));
        assert!(!message.contains("op://"));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("op.calls")).unwrap(),
        "xx"
    );
    let missing = CredentialResolver::with_op_cli(dir.path().join("missing"));
    assert!(
        missing
            .resolve("op://Agents/item/credential", Duration::from_secs(60))
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn onepassword_enforces_the_same_output_bounds_as_shell_credentials() {
    for command in [
        "printf ''",
        "printf 'a\\nb'",
        "head -c 17000 /dev/zero",
        "printf '\\377'",
    ] {
        let (_dir, cli) = mock_op(command);
        assert!(
            CredentialResolver::with_op_cli(cli)
                .resolve("op://Agents/item/credential", Duration::from_secs(60))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn concurrent_failed_refresh_is_shared_and_next_request_can_recover() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let ready = dir.path().join("ready");
    let source = format!(
        "sh://printf x >> '{}'; if test -f '{}'; then printf recovered; else sleep 0.2; printf PRIVATE_FAILURE >&2; exit 1; fi",
        calls.display(),
        ready.display()
    );
    let resolver = Arc::new(CredentialResolver::default());
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let resolver = resolver.clone();
        let source = source.clone();
        jobs.spawn(async move {
            resolver
                .resolve(&source, Duration::from_secs(60))
                .await
                .unwrap_err()
                .to_string()
        });
    }
    while let Some(result) = jobs.join_next().await {
        let error = result.unwrap();
        assert_eq!(error, "Credential command failed");
        assert!(!error.contains("PRIVATE"));
    }
    assert_eq!(std::fs::read_to_string(&calls).unwrap(), "x");
    std::fs::write(ready, "ready").unwrap();
    let value = resolver
        .resolve(&source, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(value, "recovered");
    assert!(value.is_sensitive());
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "xx");
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_credential_timeouts_do_not_serialize_into_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let source = format!(
        "sh://printf x >> '{}'; sleep 60; printf synthetic",
        calls.display()
    );
    let resolver = Arc::new(CredentialResolver::default());
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let resolver = resolver.clone();
        let source = source.clone();
        jobs.spawn(async move {
            resolver
                .resolve(&source, Duration::from_secs(60))
                .await
                .unwrap_err()
                .to_string()
        });
    }
    tokio::time::timeout(Duration::from_secs(40), async {
        while let Some(result) = jobs.join_next().await {
            assert_eq!(result.unwrap(), "Credential command timed out");
        }
    })
    .await
    .expect("waiters started serialized 30-second refresh commands");
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "x");
}
