use assert_cmd::Command;
use predicates::str::contains;
use std::env;
#[cfg(unix)]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("claude-code-proxy {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("claude-code-proxy")?;
        cmd.arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

#[test]
fn models_prints_all_providers() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.arg("models");
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex:"));
    assert!(out.contains("kimi:"));
    assert!(out.contains("opencode:"));
    assert!(out.contains("cursor:"));

    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["models", "--full"]);
    cmd.output()?;
    Ok(())
}

#[test]
fn help_describes_visible_commands_and_hides_demo() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.arg("--help");
    let output = cmd.output()?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    for description in [
        "Print version information",
        "Start the proxy server and monitor",
        "List supported provider models",
        "Manage Codex authentication",
        "Manage Kimi authentication",
        "Manage Cursor authentication",
        "Manage Grok authentication",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn unsupported_provider_auth_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["cursor", "auth", "device"]);
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(2));
    let out = String::from_utf8(output.stderr)?;
    assert!(out.contains("not yet implemented") || out.contains("unsupported"));
    Ok(())
}

#[test]
fn provider_logout_without_auth_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["kimi", "auth", "logout"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success();
    Ok(())
}

#[test]
fn antigravity_user_agent_status_is_bounded_and_logout_ignores_invalid_transport() {
    for value in ["sentinel-native-UA", "sentinel-invalid\tUA", ""] {
        let directory = TempDir::new().unwrap();
        let output = Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["antigravity", "auth", "status"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_USER_AGENT", value)
            .output()
            .unwrap();
        assert!(output.status.success());
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            status["status"]["configuration"],
            if value == "sentinel-native-UA" {
                "valid"
            } else {
                "invalid"
            }
        );
        assert_eq!(status["enabled"], false);
        assert_eq!(status["status"]["live_verified"], false);
        assert_eq!(status["backend_window_verified"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let models = Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["models", "--full"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_USER_AGENT", value)
            .output()
            .unwrap();
        assert!(models.status.success());
        let models = String::from_utf8(models.stdout).unwrap();
        assert!(!models.contains("antigravity/gemini"));
        assert!(models.contains("codex:"));
        let auth_dir = directory.path().join("antigravity");
        std::fs::create_dir(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("auth.json"), b"synthetic local credential").unwrap();
        std::fs::write(directory.path().join("other-provider-state"), b"untouched").unwrap();
        Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["antigravity", "auth", "logout"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_USER_AGENT", value)
            .assert()
            .success();
        assert!(!directory.path().join("antigravity/auth.json").exists());
        assert_eq!(
            std::fs::read(directory.path().join("other-provider-state")).unwrap(),
            b"untouched"
        );
    }
}

#[cfg(unix)]
#[test]
fn antigravity_user_agent_non_utf8_environment_fails_closed() {
    use std::os::unix::ffi::OsStringExt;
    for enabled in ["0", "1"] {
        let directory = TempDir::new().unwrap();
        let output = Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["antigravity", "auth", "status"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_ENABLED", enabled)
            .env(
                "CCP_ANTIGRAVITY_USER_AGENT",
                std::ffi::OsString::from_vec(b"sentinel-non-utf8-\xff".to_vec()),
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["status"]["configuration"], "invalid");
        assert_eq!(status["status"]["configuration_ready"], false);
        assert_eq!(status["enabled"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[test]
fn antigravity_user_agent_non_utf8_logout_still_deletes_only_local_credentials() {
    use std::os::unix::ffi::OsStringExt;
    let directory = TempDir::new().unwrap();
    let auth_dir = directory.path().join("antigravity");
    std::fs::create_dir(&auth_dir).unwrap();
    std::fs::write(auth_dir.join("auth.json"), b"synthetic-local-credential").unwrap();
    std::fs::write(directory.path().join("unrelated-state"), b"untouched").unwrap();
    let output = Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .args(["antigravity", "auth", "logout"])
        .env("CCP_CONFIG_DIR", directory.path())
        .env(
            "CCP_ANTIGRAVITY_USER_AGENT",
            std::ffi::OsString::from_vec(b"sentinel-non-utf8-\xff".to_vec()),
        )
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!auth_dir.join("auth.json").exists());
    assert_eq!(
        std::fs::read(directory.path().join("unrelated-state")).unwrap(),
        b"untouched"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sentinel"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel"));
}

#[test]
fn antigravity_manual_help_and_nonterminal_refusal() {
    let directory = TempDir::new().unwrap();
    let help = Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .args(["antigravity", "auth", "login", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--manual"));
    assert!(help.contains("experimental") || help.contains("Experimental"));
    assert!(help.contains("visible"));
    assert!(help.contains("10 columns"));
    let output = Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .args(["antigravity", "auth", "login", "--manual"])
        .env("CCP_CONFIG_DIR", directory.path())
        .env("CCP_ANTIGRAVITY_CLIENT_ID", "synthetic-public-client")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    #[cfg(unix)]
    assert!(diagnostics.contains("terminal"), "{diagnostics}");
    #[cfg(not(unix))]
    assert!(diagnostics.contains("unsupported"), "{diagnostics}");
    assert!(!diagnostics.contains("accounts.google.com"));
    assert!(!directory.path().join("antigravity").exists());
}

#[cfg(unix)]
mod manual_terminal_cli {
    use super::*;
    use std::{
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::process::CommandExt,
        },
        process::Command as ProcessCommand,
    };

    const HELPER: &str = "manual_terminal_cli::antigravity_manual_cli_background_helper";

    #[test]
    fn antigravity_manual_cli_background_helper() {
        if std::env::var_os("CCP_TEST_MANUAL_CLI_BACKGROUND").is_none() {
            return;
        }
        let status = ProcessCommand::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
            .args(["antigravity", "auth", "login", "--manual"])
            .process_group(0)
            .status()
            .unwrap();
        assert!(!status.success());
        println!("background-cli-refused");
    }

    fn mode(fd: libc::c_int) -> libc::termios {
        let mut value = std::mem::MaybeUninit::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
        unsafe { value.assume_init() }
    }

    fn set_columns(file: &File, columns: u16) {
        let size = libc::winsize {
            ws_row: 24,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
    }

    struct UnreadOutput {
        child: ChildGuard,
        _master: File,
        slave: File,
        before: libc::termios,
        home: TempDir,
    }
    impl UnreadOutput {
        fn start(invalid_configuration: bool) -> Self {
            use std::{
                ffi::CStr,
                os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
            };
            let home = TempDir::new().unwrap();
            let (mut master, mut slave) = (-1, -1);
            let mut name = [0 as libc::c_char; libc::PATH_MAX as usize];
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        name.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
            let master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            set_columns(&slave, 80);
            for file in [&master, &slave] {
                assert_eq!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                    0
                );
            }
            // A separate open file description fills this owned PTY. Never set
            // O_NONBLOCK on the stdio descriptions inherited by the real CLI:
            // doing so would hide Rust main's blocking-stderr regression.
            let name = unsafe { CStr::from_ptr(name.as_ptr()) };
            let mut filler = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
                .open(std::ffi::OsStr::from_bytes(name.to_bytes()))
                .unwrap();
            let mut full = false;
            for _ in 0..128 {
                match filler.write(&[b'x'; 1024]) {
                    Ok(n) => assert!(n > 0),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        full = true;
                        break;
                    }
                    Err(error) => panic!("could not fill private PTY: {error}"),
                }
            }
            assert!(full, "fixture did not reach output backpressure");
            drop(filler);
            assert_eq!(
                unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
                0
            );
            let before = mode(slave.as_raw_fd());
            let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
            command
                .args(["antigravity", "auth", "login", "--manual"])
                .env_clear()
                .env("HOME", home.path())
                .env("CCP_CONFIG_DIR", home.path())
                .env("XDG_STATE_HOME", home.path().join("state"))
                .env("CCP_ANTIGRAVITY_CLIENT_ID", "synthetic-public-client")
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave.try_clone().unwrap()));
            if invalid_configuration {
                command.env("CCP_ANTIGRAVITY_USER_AGENT", "invalid\tconfiguration");
            }
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = ChildGuard(command.spawn().unwrap());
            Self {
                child,
                _master: master,
                slave,
                before,
                home,
            }
        }
        fn wait_for_managed_input(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while mode(self.slave.as_raw_fd()).c_lflag & libc::ECHO != 0 {
                assert!(
                    self.child.0.try_wait().unwrap().is_none(),
                    "CLI exited before establishing terminal session"
                );
                assert!(
                    Instant::now() < deadline,
                    "CLI did not enter application-managed input mode"
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
        fn require_exit_without_drain(&mut self, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = self.child.0.try_wait().unwrap() {
                    assert_eq!(
                        status.code(),
                        Some(1),
                        "manual failure must exit nonzero normally"
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "manual CLI failed to exit while PTY output remained unread"
                );
                thread::sleep(Duration::from_millis(10));
            }
            // There is deliberately NO master.read/collect, even after exit.
            let after = mode(self.slave.as_raw_fd());
            assert_eq!(before_mode(&self.before), before_mode(&after));
            assert!(!self.home.path().join("antigravity/auth.json").exists());
        }
    }
    fn before_mode(
        mode: &libc::termios,
    ) -> (
        libc::tcflag_t,
        libc::tcflag_t,
        libc::tcflag_t,
        libc::tcflag_t,
        [libc::cc_t; libc::NCCS],
    ) {
        (
            mode.c_iflag,
            mode.c_oflag,
            mode.c_cflag,
            mode.c_lflag,
            mode.c_cc,
        )
    }

    // The shipped reqwest client must use child-only HTTPS_PROXY. This fake
    // CONNECT endpoint never tunnels, resolves Google, performs TLS, or forwards
    // a code. An accepted completion reaches it once; every refusal reaches it zero times.
    struct ConnectProbe {
        address: std::net::SocketAddr,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        worker: Option<thread::JoinHandle<()>>,
    }
    impl ConnectProbe {
        fn start() -> Self {
            use std::sync::{
                Arc, Mutex,
                atomic::{AtomicBool, Ordering},
            };
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stopped = stop.clone();
            let captured = requests.clone();
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(1)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(1)))
                                .unwrap();
                            let mut bytes = Vec::new();
                            while !bytes.ends_with(b"\r\n\r\n") {
                                let mut byte = [0];
                                if stream.read(&mut byte).unwrap_or(0) == 0 {
                                    break;
                                }
                                bytes.push(byte[0]);
                                assert!(bytes.len() <= 8192, "unbounded CONNECT request");
                            }
                            captured
                                .lock()
                                .unwrap()
                                .push(String::from_utf8(bytes).unwrap());
                            let _ = stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fake proxy accept: {error}"),
                    }
                }
            });
            Self {
                address,
                stop,
                requests,
                worker: Some(worker),
            }
        }
    }
    impl Drop for ConnectProbe {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    struct VisibleCli {
        child: ChildGuard,
        master: File,
        slave: File,
        before: libc::termios,
        output: Vec<u8>,
        home: TempDir,
        proxy: ConnectProbe,
    }
    impl VisibleCli {
        fn start() -> Self {
            Self::with_output_flag(0)
        }
        fn with_output_flag(flag: libc::tcflag_t) -> Self {
            let proxy = ConnectProbe::start();
            let home = TempDir::new().unwrap();
            let (mut master, mut slave) = (-1, -1);
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
            let master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            set_columns(&slave, 80);
            for file in [&master, &slave] {
                assert_eq!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                    0
                );
            }
            let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(
                unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            let mut before = mode(slave.as_raw_fd());
            if flag != 0 {
                before.c_oflag |= libc::OPOST | flag;
                assert_eq!(
                    unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &before) },
                    0
                );
            }
            let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
            command
                .args(["antigravity", "auth", "login", "--manual"])
                .env_clear()
                .env("HOME", home.path())
                .env("CCP_CONFIG_DIR", home.path())
                .env("XDG_STATE_HOME", home.path().join("state"))
                .env("CCP_ANTIGRAVITY_CLIENT_ID", "synthetic-public-client")
                .env("HTTPS_PROXY", format!("http://{}", proxy.address))
                .env("NO_PROXY", "localhost,127.0.0.1,::1,[::1]")
                .env("RUST_LOG", "trace")
                .env("CCP_LOG_STDERR", "1")
                .env("CCP_LOG_VERBOSE", "1")
                .env("CCP_TRAFFIC_LOG", "1")
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(
                    File::create(home.path().join("stdout")).unwrap(),
                ))
                .stderr(Stdio::from(
                    File::create(home.path().join("stderr")).unwrap(),
                ));
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = ChildGuard(command.spawn().unwrap());
            Self {
                child,
                master,
                slave,
                before,
                output: Vec::new(),
                home,
                proxy,
            }
        }
        fn collect(&mut self) {
            let mut bytes = [0; 4096];
            for _ in 0..16 {
                match self.master.read(&mut bytes) {
                    Ok(n) if n > 0 => self.output.extend_from_slice(&bytes[..n]),
                    Ok(_) => break,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(libc::EIO) =>
                    {
                        break;
                    }
                    Err(error) => panic!("visible CLI read: {error}"),
                }
            }
            assert!(self.output.len() < 256 * 1024);
        }
        fn until(&mut self, needle: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.collect();
                if String::from_utf8_lossy(&self.output).contains(needle) {
                    return;
                }
                assert!(
                    self.child.0.try_wait().unwrap().is_none(),
                    "CLI exited before {needle}"
                );
                assert!(Instant::now() < deadline, "CLI did not display {needle}");
                thread::sleep(Duration::from_millis(5));
            }
        }
        fn send(&mut self, input: &[u8]) {
            assert_eq!(self.master.write(input).unwrap(), input.len());
        }
        fn block_output(&self) {
            use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
            let mut name = [0 as libc::c_char; 4096];
            assert_eq!(
                unsafe { libc::ttyname_r(self.slave.as_raw_fd(), name.as_mut_ptr(), name.len()) },
                0
            );
            let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
            let mut filler = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
                .open(std::ffi::OsStr::from_bytes(name.to_bytes()))
                .unwrap();
            for _ in 0..256 {
                match filler.write(&[b'!'; 1024]) {
                    Ok(n) => assert!(n > 0),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                    other => panic!("PTY fill failed: {other:?}"),
                }
            }
            panic!("fixture did not reach output backpressure");
        }
        fn finish(&mut self, message: &str, connects: usize, sentinel: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.collect();
                if let Some(status) = self.child.0.try_wait().unwrap() {
                    assert_eq!(status.code(), Some(1));
                    break;
                }
                assert!(Instant::now() < deadline, "CLI did not finish");
                thread::sleep(Duration::from_millis(5));
            }
            self.collect();
            let text = String::from_utf8_lossy(&self.output);
            assert!(text.contains(message), "{text}");
            assert!(!text.contains("QUEUED_PRIVATE_TAIL"));
            assert_eq!(
                before_mode(&self.before),
                before_mode(&mode(self.slave.as_raw_fd()))
            );
            let mut pending: libc::c_int = -1;
            assert_eq!(
                unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::FIONREAD, &mut pending) },
                0
            );
            assert_eq!(pending, 0);
            for name in ["stdout", "stderr"] {
                let log = std::fs::read_to_string(self.home.path().join(name)).unwrap();
                assert!(!log.contains(sentinel), "redirected stream disclosed input");
                assert!(!log.contains("QUEUED_PRIVATE_TAIL"));
            }
            assert!(!self.home.path().join("antigravity/auth.json").exists());
            assert!(
                !self
                    .home
                    .path()
                    .join("state/claude-code-proxy/traffic")
                    .exists()
            );
            let requests = self.proxy.requests.lock().unwrap();
            assert_eq!(requests.len(), connects);
            for request in requests.iter() {
                assert_eq!(
                    request.lines().next(),
                    Some("CONNECT oauth2.googleapis.com:443 HTTP/1.1")
                );
                assert!(!request.contains(sentinel));
                assert!(!request.contains("code_verifier"));
            }
        }
    }

    #[test]
    fn antigravity_manual_cli_visible_before_enter_and_exact_https_proxy_boundary() {
        let code = "4/0cli-visible-private";
        for completion in ["submit", "ctrl-c", "invalid", "escape", "resize"] {
            let mut child = VisibleCli::start();
            child.until("Paste the displayed authorization code");
            child.send(code.as_bytes());
            child.until(&format!("[{}] {code}", code.len()));
            assert_eq!(mode(child.slave.as_raw_fd()).c_lflag & libc::ECHO, 0);
            assert!(String::from_utf8_lossy(&child.output).contains("sensitive"));
            assert!(child.proxy.requests.lock().unwrap().is_empty());
            let (message, connects) = match completion {
                "submit" => {
                    child.send(b"\r\nQUEUED_PRIVATE_TAIL\n");
                    ("OAuth token exchange transport failed", 1)
                }
                "ctrl-c" => {
                    child.send(b"\x03QUEUED_PRIVATE_TAIL\n");
                    ("Manual OAuth login cancelled", 0)
                }
                "invalid" => {
                    child.send(b"%\nQUEUED_PRIVATE_TAIL\n");
                    ("Invalid OAuth completion", 0)
                }
                "escape" => {
                    child.send(b"\x1b]52;c;injected\x07QUEUED_PRIVATE_TAIL\n");
                    ("Invalid OAuth completion", 0)
                }
                _ => {
                    set_columns(&child.slave, 40);
                    ("Manual OAuth terminal width changed; login cancelled", 0)
                }
            };
            child.finish(message, connects, code);
        }
    }

    #[test]
    fn antigravity_manual_cli_visible_output_translation_refuses_without_connect() {
        let flags = [
            libc::OCRNL,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::OLCUC,
        ];
        for flag in flags {
            let mut child = VisibleCli::with_output_flag(flag);
            let message =
                "Manual OAuth terminal output must preserve carriage returns and letter case";
            child.finish(
                if flag == libc::OCRNL {
                    message
                } else {
                    "MANUAL OAUTH TERMINAL OUTPUT MUST PRESERVE CARRIAGE RETURNS AND LETTER CASE"
                },
                0,
                "synthetic-code-not-entered",
            );
            assert!(
                !String::from_utf8_lossy(&child.output)
                    .to_lowercase()
                    .contains("accounts.google.com")
            );
        }
    }

    #[test]
    fn antigravity_manual_cli_visible_echo_backpressure_exits_without_drain_or_connect() {
        let code = "4/0cli-blocked-private";
        for cancellation in ["key", "signal", "resize"] {
            let mut child = VisibleCli::start();
            child.until("[0] ");
            child.block_output();
            child.send(code.as_bytes());
            thread::sleep(Duration::from_millis(30));
            match cancellation {
                "key" => child.send(b"\x03QUEUED_PRIVATE_TAIL\n"),
                "signal" => send_signal(&child.child, "-TERM").unwrap(),
                _ => set_columns(&child.slave, 40),
            }
            let deadline = Instant::now() + Duration::from_secs(4);
            while child.child.0.try_wait().unwrap().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "blocked echo/diagnostic prevented CLI exit"
                );
                thread::sleep(Duration::from_millis(5));
            }
            // Diagnostic may be undeliverable; no output was drained to let it finish.
            child.finish("", 0, code);
        }
    }

    #[test]
    fn antigravity_manual_cli_unread_output_cancellation_exits() {
        for signal in ["-INT", "-TERM"] {
            let mut child = UnreadOutput::start(false);
            child.wait_for_managed_input();
            send_signal(&child.child, signal).unwrap();
            child.require_exit_without_drain(Duration::from_secs(5));
        }
    }

    #[test]
    fn antigravity_manual_cli_unread_output_configuration_error_exits() {
        let mut child = UnreadOutput::start(true);
        child.require_exit_without_drain(Duration::from_secs(5));
    }

    #[test]
    fn antigravity_manual_cli_unread_output_real_deadline_exits() {
        let mut child = UnreadOutput::start(false);
        child.wait_for_managed_input();
        // Intentionally use the REAL production deadline. No test-only timeout
        // switch exists in the shipped command, and no PTY output is drained.
        child.require_exit_without_drain(Duration::from_secs(185));
    }

    #[test]
    fn antigravity_manual_cli_real_terminal_cancellation_and_background_refusal() {
        for background in [false, true] {
            let home = TempDir::new().unwrap();
            let (mut master, mut slave) = (-1, -1);
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
            let mut master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            set_columns(&slave, 80);
            for file in [&master, &slave] {
                assert_eq!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                    0
                );
            }
            let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(
                unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            let before = mode(slave.as_raw_fd());
            let mut command = if background {
                let mut command = ProcessCommand::new(std::env::current_exe().unwrap());
                command.args([HELPER, "--exact", "--nocapture"]);
                command
            } else {
                let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
                command.args(["antigravity", "auth", "login", "--manual"]);
                command
            };
            command
                .env_clear()
                .env("HOME", home.path())
                .env("CCP_CONFIG_DIR", home.path())
                .env("XDG_STATE_HOME", home.path().join("state"))
                .env("CCP_ANTIGRAVITY_CLIENT_ID", "synthetic-public-client")
                .env("CCP_TEST_MANUAL_CLI_BACKGROUND", "1")
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::from(slave.try_clone().unwrap()))
                .stderr(Stdio::from(slave.try_clone().unwrap()));
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = ChildGuard(command.spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut output = Vec::new();
            let mut cancelled = false;
            loop {
                let mut buffer = [0u8; 4096];
                match master.read(&mut buffer) {
                    Ok(n) => output.extend_from_slice(&buffer[..n]),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.raw_os_error() == Some(libc::EIO) => {}
                    Err(error) => panic!("PTY read failed: {error}"),
                }
                let text = String::from_utf8_lossy(&output);
                if !background
                    && !cancelled
                    && text.contains("Paste the displayed authorization code")
                {
                    assert_eq!(mode(slave.as_raw_fd()).c_lflag & libc::ECHO, 0);
                    send_signal(&child, "-TERM").unwrap();
                    cancelled = true;
                }
                if let Some(status) = child.0.try_wait().unwrap() {
                    // Consume the bounded final diagnostic after process exit.
                    loop {
                        match master.read(&mut buffer) {
                            Ok(n) if n > 0 => output.extend_from_slice(&buffer[..n]),
                            _ => break,
                        }
                    }
                    assert_eq!(status.success(), background);
                    break;
                }
                assert!(Instant::now() < deadline, "real manual CLI failed to exit");
                thread::sleep(Duration::from_millis(5));
            }
            let output = String::from_utf8_lossy(&output);
            if background {
                assert!(
                    output.contains("foreground controlling terminal"),
                    "{output}"
                );
                assert!(!output.contains("accounts.google.com"));
                assert!(!home.path().join("antigravity").exists());
            } else {
                assert!(cancelled);
                assert!(output.contains("Manual OAuth login cancelled"), "{output}");
                assert!(output.contains("CODE DISPLAYED"));
                assert!(!home.path().join("antigravity/auth.json").exists());
            }
            let after = mode(slave.as_raw_fd());
            assert_eq!(before.c_iflag, after.c_iflag);
            assert_eq!(before.c_oflag, after.c_oflag);
            assert_eq!(before.c_cflag, after.c_cflag);
            assert_eq!(before.c_lflag, after.c_lflag);
            assert_eq!(before.c_cc, after.c_cc);
        }
    }
}

#[test]
fn models_output_is_stable_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["models", "--full"]);
    let output = cmd.output()?;
    let out = String::from_utf8(output.stdout)?;
    let codex_pos = out.find("codex:").unwrap_or(0);
    let kimi_pos = out.find("kimi:").unwrap_or(0);
    let cursor_pos = out.find("cursor:").unwrap_or(0);
    assert!(codex_pos < kimi_pos);
    assert!(kimi_pos < cursor_pos);
    Ok(())
}

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_service(
    child: &mut ChildGuard,
    port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                if let Some(status) = child.0.try_wait()? {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.0.stderr.take() {
                        pipe.read_to_string(&mut stderr)?;
                    }
                    return Err(format!("service exited with {status}: {stderr}").into());
                }
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(unix)]
fn send_signal(child: &ChildGuard, signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("kill")
        .args([signal, &child.0.id().to_string()])
        .status()?;
    if !status.success() {
        return Err(format!("kill {signal} failed with {status}").into());
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("plain service did not exit after the second signal".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn antigravity_user_agent_invalid_environment_is_scoped_to_native_provider()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
    for enabled in ["0", "1"] {
        for invalid in [
            b"sentinel-invalid\tUA".to_vec(),
            b"sentinel-invalid-\xff".to_vec(),
        ] {
            let directory = TempDir::new()?;
            let upstream = TcpListener::bind("127.0.0.1:0")?;
            upstream.set_nonblocking(true)?;
            let config = serde_json::json!({"antigravity": {
                "userAgent": "file-native-fallback",
                "baseUrl": format!("https://{}", upstream.local_addr()?)
            }});
            std::fs::write(directory.path().join("config.json"), config.to_string())?;
            let auth_dir = directory.path().join("antigravity");
            std::fs::create_dir(&auth_dir)?;
            std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700))?;
            let auth_path = auth_dir.join("auth.json");
            std::fs::write(&auth_path, serde_json::json!({
                "profile_id":"synthetic-profile", "project_id":"synthetic-project",
                "access_token":"synthetic-access", "refresh_token":null, "expires_at":4102444800u64
            }).to_string())?;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))?;
            let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let child = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
                .args(["serve", "--no-monitor", "--port", &port.to_string()])
                .env("CCP_CONFIG_DIR", directory.path())
                .env("XDG_STATE_HOME", directory.path().join("state"))
                .env("CCP_ANTIGRAVITY_ENABLED", enabled)
                .env(
                    "CCP_ANTIGRAVITY_USER_AGENT",
                    std::ffi::OsString::from_vec(invalid),
                )
                .env("CCP_CURSOR_CLIENT_VERSION", "synthetic-version")
                .env("CCP_LOG_STDERR", "1")
                .env("CCP_LOG_VERBOSE", "1")
                .env("CCP_TRAFFIC_LOG", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut child = ChildGuard(child);
            drop(wait_for_service(&mut child, port)?);
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()?;
            let response = client
                .get(format!(
                    "http://127.0.0.1:{port}/v1/models?include_capabilities=true"
                ))
                .send()
                .await?;
            assert_eq!(response.status(), 200);
            let models: serde_json::Value = response.json().await?;
            assert!(!models["data"].as_array().unwrap().iter().any(|entry| {
                entry["id"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("antigravity/"))
            }));
            let capability = &models["capabilities"]["providers"]["antigravity"];
            assert_eq!(capability["status"]["configuration"], "invalid");
            assert_eq!(capability["status"]["configuration_ready"], false);
            assert_eq!(capability["enabled"], false);
            // A network-free other-provider operation must reach traffic setup,
            // not merely the early rejection of an unavailable native model.
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
                .json(&serde_json::json!({
                    "model": "kimi-for-coding",
                    "messages": [{"role": "user", "content": "synthetic"}]
                }))
                .send()
                .await?;
            assert_eq!(response.status(), 200);
            let count: serde_json::Value = response.json().await?;
            assert!(
                count["input_tokens"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
            );
            assert!(
                directory
                    .path()
                    .join("state/claude-code-proxy/traffic")
                    .exists()
            );
            let response = client.post(format!("http://127.0.0.1:{port}/v1/messages"))
                .json(&serde_json::json!({"model":"antigravity/gemini-4-argon-low", "max_tokens":32, "messages":[{"role":"user","content":"synthetic"}]}))
                .send().await?;
            assert!(response.status().is_client_error());
            assert!(!response.text().await?.contains("sentinel"));
            assert!(
                matches!(upstream.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
            send_signal(&child, "-TERM")?;
            assert!(wait_for_exit(&mut child, Duration::from_secs(5))?.success());
            let mut stderr = String::new();
            child.0.stderr.take().unwrap().read_to_string(&mut stderr)?;
            assert!(!stderr.contains("sentinel"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn plain_service_exits_on_second_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let upstream = TcpListener::bind("127.0.0.1:0")?;
    let upstream_url = format!("http://{}", upstream.local_addr()?);
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fixture = thread::spawn(move || {
        let (stream, _) = upstream.accept().unwrap();
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(stream);
    });

    let config = TempDir::new()?;
    let auth_dir = config.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"test","refresh":"test","expires":4102444800000,"scope":"openid","userId":"test"}"#,
    )?;
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
        .args(["serve", "--no-monitor", "--port", &port.to_string()])
        .env("CCP_CONFIG_DIR", config.path())
        .env("CCP_KIMI_BASE_URL", upstream_url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard(child);
    let mut downstream = wait_for_service(&mut child, port)?;
    let body = br#"{"model":"kimi-for-coding","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    write!(
        downstream,
        "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    downstream.write_all(body)?;
    accepted_rx.recv_timeout(Duration::from_secs(20))?;

    send_signal(&child, signal)?;
    thread::sleep(Duration::from_millis(200));
    assert!(child.0.try_wait()?.is_none());
    send_signal(&child, signal)?;
    let status = wait_for_exit(&mut child, Duration::from_secs(2));
    let _ = release_tx.send(());
    fixture.join().unwrap();

    assert_eq!(status?.code(), Some(130));
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_ctrl_c() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-INT")
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_sigterm() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-TERM")
}

#[test]
fn kimi_auth_status_reads_stored_auth() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let auth_dir = temp.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"a","refresh":"r","expires":4102444800000,"scope":"openid","userId":"u"}"#,
    )?;
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["kimi", "auth", "status"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success().stdout(contains("User: u"));
    Ok(())
}
