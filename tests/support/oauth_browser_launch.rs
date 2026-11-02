use super::open_login_url;

const LOGIN_URLS: [&str; 2] = [
    "https://auth.x.ai/oauth2/authorize?response_type=code&client_id=fixture-client&redirect_uri=http%3A%2F%2F127.0.0.1%3A12345%2Fcallback&scope=openid+profile&code_challenge=ab^cd|ef(gh)<ij>&code_challenge_method=S256&state=a%26b",
    "https://cursor.com/loginDeepControl?challenge=ab^cd|ef(gh)<ij>&uuid=12345678-abcd-4321-abcd-123456789abc&mode=login&redirectTarget=cli%3A%2F%2Fcallback",
];

#[test]
fn oauth_browser_launch_keeps_urls_out_of_cmd() {
    for url in LOGIN_URLS {
        let error = open_login_url(url, "windows", |command| {
            assert_eq!(
                command.get_program(),
                "rundll32.exe",
                "OAuth URLs must bypass cmd"
            );
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["url.dll,FileProtocolHandler", url]
            );
            Err(std::io::Error::other("browser launch intercepted"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "browser launch intercepted");
    }
}

#[test]
fn oauth_browser_launch_preserves_unix_arguments() {
    for (platform, program) in [("macos", "open"), ("linux", "xdg-open")] {
        for url in LOGIN_URLS {
            let error = open_login_url(url, platform, |command| {
                assert_eq!(command.get_program(), program);
                assert_eq!(command.get_args().collect::<Vec<_>>(), [url]);
                Err(std::io::Error::other("browser launch intercepted"))
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "browser launch intercepted");
        }
    }
}

#[cfg(windows)]
#[test]
fn oauth_browser_launch_roundtrips_through_native_process() {
    // Substitute a native argv recorder for rundll32 so no browser is opened.
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("argv.rs");
    let recorder = temp.path().join("argv.exe");
    std::fs::write(
        &source,
        r#"fn main() {
            for arg in std::env::args().skip(1) {
                println!("{arg}");
            }
        }"#,
    )
    .unwrap();
    let compile = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&recorder)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "argv recorder did not compile: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    for url in LOGIN_URLS {
        let error = open_login_url(url, "windows", |command| {
            assert_eq!(
                command.get_program(),
                "rundll32.exe",
                "OAuth URLs must bypass cmd"
            );
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["url.dll,FileProtocolHandler", url]
            );
            let output = std::process::Command::new(&recorder)
                .args(command.get_args())
                .output()?;
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                stdout.lines().collect::<Vec<_>>(),
                ["url.dll,FileProtocolHandler", url]
            );
            Err(std::io::Error::other("browser launch intercepted"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "browser launch intercepted");
    }
}
