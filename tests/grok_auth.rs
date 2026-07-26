use assert_cmd::Command;
use tempfile::TempDir;

fn grok_cmd(command: &str) -> (Command, TempDir) {
    let temp = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("claude-code-proxy").unwrap();
    cmd.args(["grok", "auth", command]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.env("HOME", temp.path());
    (cmd, temp)
}

#[test]
fn grok_auth_status_reads_isolated_file_credentials() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, temp) = grok_cmd("status");
    let auth_dir = temp.path().join("grok");
    let auth_path = auth_dir.join("auth.json");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        &auth_path,
        r#"{"access":"a","refresh":"r","expires_at_ms":4102444800000,"issuer":"https://auth.x.ai","client_id":"client"}"#,
    )?;

    let output = cmd.assert().success().get_output().stdout.clone();
    let out = String::from_utf8(output)?;
    assert!(out.contains(&format!("Auth path: {}", auth_path.display())));
    assert!(out.contains("Authenticated: true"));
    assert!(out.contains("Expires in "));
    Ok(())
}

#[test]
fn grok_auth_status_without_credentials_is_nonzero() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, _temp) = grok_cmd("status");
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8(output.stdout)?, "Not authenticated\n");
    Ok(())
}

#[test]
fn grok_auth_logout_removes_only_isolated_credentials() -> Result<(), Box<dyn std::error::Error>> {
    let (mut cmd, temp) = grok_cmd("logout");
    let auth_dir = temp.path().join("grok");
    let auth_path = auth_dir.join("auth.json");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        &auth_path,
        r#"{"access":"a","refresh":"r","expires_at_ms":4102444800000,"issuer":"https://auth.x.ai","client_id":"client"}"#,
    )?;

    cmd.assert().success();
    assert!(!auth_path.exists());
    Ok(())
}

#[test]
fn isolated_logout_never_deletes_default_home_credentials() -> Result<(), Box<dyn std::error::Error>>
{
    let isolated = TempDir::new()?;
    let home = TempDir::new()?;
    let real_auth = home.path().join(".config/claude-code-proxy/grok/auth.json");
    std::fs::create_dir_all(real_auth.parent().expect("auth parent"))?;
    std::fs::write(
        &real_auth,
        r#"{"access":"real","refresh":"real-refresh","expires_at_ms":4102444800000,"issuer":"https://auth.x.ai","client_id":"client"}"#,
    )?;

    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["grok", "auth", "logout"])
        .env("CCP_CONFIG_DIR", isolated.path())
        .env("HOME", home.path())
        .assert()
        .success();

    assert!(
        real_auth.exists(),
        "CCP_CONFIG_DIR must isolate legacy credential cleanup from HOME"
    );
    Ok(())
}
