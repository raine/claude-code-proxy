//! Explicit monitor control requests, kept off the terminal event loop.
use std::{sync::mpsc, time::Duration};

use anyhow::{Result, bail};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountChoice {
    pub provider: String,
    pub name: String,
}

impl AccountChoice {
    pub fn label(&self) -> String {
        format!("{} / {}", self.provider, self.name)
    }
}

fn switch_endpoint(base: &str) -> Result<Url> {
    let mut url = Url::parse(base)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("unsupported monitor control URL");
    }
    // The integrated monitor advertises the listener address; connect locally
    // when it listens on all interfaces. Attached monitors retain their URL.
    match url.host() {
        Some(url::Host::Ipv4(ip)) if ip.is_unspecified() => {
            url.set_host(Some("127.0.0.1"))?;
        }
        Some(url::Host::Ipv6(ip)) if ip.is_unspecified() => {
            url.set_host(Some("[::1]"))?;
        }
        _ => {}
    }
    Ok(url.join("monitor/accounts/switch")?)
}

fn switch_account(base: &str, choice: &AccountChoice) -> Result<()> {
    let endpoint = switch_endpoint(base)?;
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .post(endpoint)
        .header("X-Proxy-Account-Switch", "1")
        .json(choice)
        .send()
        .map_err(|_| anyhow::anyhow!("Cannot reach proxy; switch outcome unknown. Check selected profile before retrying."))?;
    match response.status().as_u16() {
        200 => {
            let selected: AccountChoice = response.json().map_err(|_| {
                anyhow::anyhow!("Invalid switch reply; check selected profile before retrying.")
            })?;
            if selected != *choice {
                bail!("Unexpected switch reply; check selected profile before retrying.");
            }
            Ok(())
        }
        409 => bail!("Proxy is busy. Wait for active requests to finish, then retry."),
        400 | 422 => bail!("Profile is missing, invalid, or has no usable local credentials."),
        403 => bail!("Switch denied. Connect through loopback or an SSH tunnel."),
        404 | 405 => {
            bail!("This proxy does not support runtime switching; update the proxy executable.")
        }
        _ => bail!("Switch failed. Check selected profile and proxy logs before retrying."),
    }
}

/// One explicit action at a time; dropping the dashboard never blocks on I/O.
#[derive(Default)]
pub struct AccountSwitchControl {
    pending: Option<mpsc::Receiver<Result<(), String>>>,
}

impl AccountSwitchControl {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn start(&mut self, base: String, choice: AccountChoice) {
        if self.is_pending() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        self.pending = Some(receiver);
        std::thread::spawn(move || {
            let result = switch_account(&base, &choice).map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
    }

    pub fn poll(&mut self) -> Option<Result<(), String>> {
        let result = match self.pending.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Switch worker stopped; check selected profile before retrying.".to_owned())
            }
        };
        self.pending = None;
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_urls_use_loopback_for_unspecified_listeners() {
        assert_eq!(
            switch_endpoint("http://0.0.0.0:18765").unwrap().as_str(),
            "http://127.0.0.1:18765/monitor/accounts/switch"
        );
        assert_eq!(
            switch_endpoint("http://[::]:18765").unwrap().as_str(),
            "http://[::1]:18765/monitor/accounts/switch"
        );
        assert!(switch_endpoint("mock://tui-demo").is_err());
        assert!(switch_endpoint("http://user:secret@localhost/").is_err());
        assert!(switch_endpoint("http://localhost/?key=secret").is_err());
    }

    #[test]
    fn switch_request_has_explicit_header_and_reports_busy_without_body() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.starts_with("post /monitor/accounts/switch "));
            assert!(request.contains("x-proxy-account-switch: 1\r\n"));
            assert!(!request.contains("\r\norigin:"));
            stream
                .write_all(
                    b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let error = switch_account(
            &base,
            &AccountChoice {
                provider: "codex".to_owned(),
                name: "work".to_owned(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("busy"));
        server.join().unwrap();
    }
}
