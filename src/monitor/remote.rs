use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Url};
use tokio::{sync::watch, task::JoinHandle, time::MissedTickBehavior};

use super::snapshot::{MonitorResponse, MonitorSnapshot, PROTOCOL_VERSION, SnapshotUpdate};

const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Owns polling; dropping the dashboard cancels only its outstanding read.
pub struct RemoteMonitor {
    updates: watch::Receiver<SnapshotUpdate>,
    poller: JoinHandle<()>,
}

impl RemoteMonitor {
    pub async fn connect(client: Client, base_url: Url) -> Result<Self> {
        if !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            bail!("monitor URL must be an HTTP(S) base URL without credentials, query or fragment");
        }
        let endpoint = base_url.join("monitor").context("invalid monitor URL")?;
        let initial = fetch_snapshot(&client, &endpoint)
            .await
            .context("cannot attach to proxy monitor")?;
        let (sender, updates) = watch::channel(SnapshotUpdate::Live(initial.clone()));
        let poller = tokio::spawn(async move {
            let mut current = SnapshotUpdate::Live(initial);
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                let result = fetch_snapshot(&client, &endpoint)
                    .await
                    .map_err(|error| error.to_string());
                current = current.updated(result);
                if sender.send(current.clone()).is_err() {
                    break;
                }
            }
        });
        Ok(Self { updates, poller })
    }

    pub fn snapshot(&self) -> SnapshotUpdate {
        self.updates.borrow().clone()
    }
}

impl Drop for RemoteMonitor {
    fn drop(&mut self) {
        self.poller.abort();
    }
}

async fn fetch_snapshot(client: &Client, endpoint: &Url) -> Result<MonitorSnapshot> {
    let mut response = client
        .get(endpoint.clone())
        .send()
        .await?
        .error_for_status()?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_SNAPSHOT_BYTES {
            bail!("monitor snapshot exceeds the size limit");
        }
        body.extend_from_slice(&chunk);
    }
    let response: MonitorResponse =
        serde_json::from_slice(&body).context("invalid monitor snapshot")?;
    if response.version != PROTOCOL_VERSION {
        bail!(
            "unsupported monitor protocol version {}; expected {}",
            response.version,
            PROTOCOL_VERSION
        );
    }
    Ok(response.snapshot)
}
