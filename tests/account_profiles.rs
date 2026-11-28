use assert_cmd::Command;
use claude_code_proxy::account_profiles::{ProfileStore, save_anthropic_key};
use predicates::prelude::*;
use tempfile::TempDir;

fn proxy(dir: &TempDir) -> Command {
    let mut command = Command::cargo_bin("claude-code-proxy").unwrap();
    command
        .env("CCP_CONFIG_DIR", dir.path())
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env_remove("CCP_CODEX_PROFILE")
        .env_remove("CCP_ANTHROPIC_PROFILE")
        .env_remove("CCP_ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_API_KEY");
    command
}

#[test]
fn anthropic_profiles_can_be_added_selected_listed_and_removed() {
    let dir = TempDir::new().unwrap();
    proxy(&dir)
        .args(["anthropic", "profile", "add", "work"])
        .write_stdin("work-secret\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("Saved Anthropic profile: work"))
        .stdout(predicate::str::contains("work-secret").not());
    proxy(&dir)
        .args(["anthropic", "auth", "login", "--profile", "personal"])
        .env("CCP_ANTHROPIC_API_KEY", "personal-secret")
        .assert()
        .success();
    proxy(&dir)
        .args(["anthropic", "profile", "use", "work"])
        .assert()
        .success();
    proxy(&dir)
        .args(["anthropic", "profile", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("work (default)"))
        .stdout(predicate::str::contains("personal"))
        .stdout(predicate::str::contains("secret").not());
    proxy(&dir)
        .args(["anthropic", "auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile work:"));
    proxy(&dir)
        .args(["anthropic", "auth", "status", "--profile", "personal"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile personal:"));
    proxy(&dir)
        .args(["anthropic", "auth", "status"])
        .env("CCP_ANTHROPIC_PROFILE", "personal")
        .assert()
        .success()
        .stdout(predicate::str::contains("profile personal:"));
    proxy(&dir)
        .args(["anthropic", "auth", "status", "--profile", "work"])
        .env("CCP_ANTHROPIC_PROFILE", "personal")
        .assert()
        .success()
        .stdout(predicate::str::contains("profile work:"));
    proxy(&dir)
        .args(["anthropic", "profile", "remove", "work", "--yes"])
        .assert()
        .failure();
    proxy(&dir)
        .args(["anthropic", "profile", "remove", "personal"])
        .assert()
        .failure();
    proxy(&dir)
        .args(["anthropic", "profile", "remove", "personal", "--yes"])
        .assert()
        .success();
    assert!(!dir.path().join("accounts/anthropic/personal.json").exists());
    assert!(dir.path().join("accounts/anthropic/work.json").exists());
}

#[test]
fn missing_named_anthropic_profile_never_uses_default_key() {
    let dir = TempDir::new().unwrap();
    proxy(&dir)
        .args(["anthropic", "auth", "status", "--profile", "missing"])
        .env("CCP_ANTHROPIC_API_KEY", "default-secret")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Not authenticated"));
    proxy(&dir)
        .args(["anthropic", "auth", "status", "--profile", "default"])
        .env("CCP_ANTHROPIC_API_KEY", "default-secret")
        .assert()
        .success();
}

#[test]
fn invalid_names_fail_before_authentication_or_file_access() {
    let dir = TempDir::new().unwrap();
    for name in ["../outside", "work/other", "con", "nul", "BadName"] {
        proxy(&dir)
            .args(["codex", "auth", "login", "--profile", name])
            .assert()
            .failure();
        proxy(&dir)
            .args(["anthropic", "profile", "add", name])
            .write_stdin("secret")
            .assert()
            .failure();
    }
    proxy(&dir)
        .args(["kimi", "profile", "list"])
        .assert()
        .failure();
}

#[test]
fn codex_profiles_do_not_read_or_delete_other_account_credentials() {
    let dir = TempDir::new().unwrap();
    let store = ProfileStore::new(dir.path().to_owned());
    for name in ["work", "personal"] {
        let path = store.credential_path("codex", name).unwrap();
        claude_code_proxy::auth::write_atomically(
            &path.to_string_lossy(),
            &serde_json::json!({
                "access": format!("{name}-access"), "refresh": format!("{name}-refresh"),
                "expires": 4102444800000_u64, "accountId": format!("{name}-account")
            }),
        )
        .unwrap();
    }
    proxy(&dir)
        .args(["codex", "auth", "status", "--profile", "work"])
        .assert()
        .success()
        .stdout(predicate::str::contains("work-account"))
        .stdout(predicate::str::contains("personal-account").not())
        .stdout(predicate::str::contains("work-refresh").not());
    proxy(&dir)
        .args(["codex", "auth", "status", "--profile", "missing"])
        .assert()
        .code(1);
    proxy(&dir)
        .args(["codex", "profile", "use", "personal"])
        .assert()
        .success();
    proxy(&dir)
        .args(["codex", "auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("personal-account"));
    proxy(&dir)
        .args(["codex", "auth", "logout", "--profile", "work"])
        .assert()
        .success();
    assert!(!store.credential_path("codex", "work").unwrap().exists());
    assert!(store.credential_path("codex", "personal").unwrap().exists());
}

struct ProxyChild(std::process::Child);

impl Drop for ProxyChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn running_proxy_keeps_its_account_after_saved_default_changes() {
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    type Seen = Arc<Mutex<Vec<String>>>;
    async fn upstream(State(seen): State<Seen>, headers: HeaderMap) -> Json<serde_json::Value> {
        seen.lock()
            .unwrap()
            .push(headers["x-api-key"].to_str().unwrap().to_owned());
        Json(serde_json::json!({
            "id": "msg_profile", "type": "message", "role": "assistant",
            "model": "claude-opus-5-5", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
    }

    let dir = TempDir::new().unwrap();
    let store = ProfileStore::new(dir.path().to_owned());
    save_anthropic_key(&store, "work", "work-test-key").unwrap();
    save_anthropic_key(&store, "personal", "personal-test-key").unwrap();
    store.use_profile("anthropic", "work").unwrap();
    let seen: Seen = Default::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/messages", post(upstream))
        .with_state(seen.clone());
    let upstream_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port().to_string();
    drop(reservation);
    let mut child = ProxyChild(
        std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
            .args([
                "serve",
                "--no-monitor",
                "--port",
                &port,
                "--anthropic-profile",
                "work",
            ])
            .env("CCP_CONFIG_DIR", dir.path())
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("CCP_BIND_ADDRESS", "127.0.0.1")
            .env("CCP_ANTHROPIC_BASE_URL", &upstream_url)
            .env("CCP_CODEX_PROFILE", "default")
            .env_remove("CCP_ANTHROPIC_PROFILE")
            .env_remove("CCP_ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env("NO_PROXY", "*")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "proxy exited before becoming ready"
            );
            if client.get(format!("{base}/v1/models")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("proxy startup timed out");
    for change_default in [false, true] {
        if change_default {
            store.use_profile("anthropic", "personal").unwrap();
        }
        let response = client
            .post(format!("{base}/v1/messages"))
            .json(&serde_json::json!({
                "model": "anthropic/claude-opus-5-5", "max_tokens": 32,
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        response.bytes().await.unwrap();
    }
    assert_eq!(*seen.lock().unwrap(), ["work-test-key", "work-test-key"]);
    upstream_task.abort();
}

#[test]
fn saved_key_profile_ignores_a_different_environment_key() {
    let dir = TempDir::new().unwrap();
    let store = ProfileStore::new(dir.path().to_owned());
    save_anthropic_key(&store, "work", "work-secret").unwrap();
    store.use_profile("anthropic", "work").unwrap();
    // A missing named file must fail closed even when default credentials exist.
    std::fs::remove_file(store.credential_path("anthropic", "work").unwrap()).unwrap();
    proxy(&dir)
        .args(["anthropic", "auth", "status"])
        .env("CCP_ANTHROPIC_API_KEY", "unrelated-secret")
        .assert()
        .code(1);
}
