use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex;

use super::client::{OpenCodeClient, OpenCodeError, OpenCodeUsageResponse, OpenCodeUsageWindow};

const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct OpenCodeUsageSnapshot {
    pub response: OpenCodeUsageResponse,
    fetched_at: OffsetDateTime,
}

struct CacheEntry {
    snapshot: OpenCodeUsageSnapshot,
    stored_at: Instant,
}

pub struct OpenCodeUsageService {
    client: OpenCodeClient,
    cache: Mutex<Option<CacheEntry>>,
    ttl: Duration,
}

impl OpenCodeUsageService {
    pub fn new(client: OpenCodeClient) -> Self {
        Self::with_ttl(client, DEFAULT_CACHE_TTL)
    }

    fn with_ttl(client: OpenCodeClient, ttl: Duration) -> Self {
        Self {
            client,
            cache: Mutex::new(None),
            ttl,
        }
    }

    pub async fn get(&self) -> Result<OpenCodeUsageSnapshot, OpenCodeError> {
        self.get_at(Instant::now()).await
    }

    async fn get_at(&self, now: Instant) -> Result<OpenCodeUsageSnapshot, OpenCodeError> {
        // Hold the lock through refresh so concurrent dashboard polls coalesce.
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.as_ref()
            && now.saturating_duration_since(entry.stored_at) < self.ttl
        {
            return Ok(entry.snapshot.clone());
        }

        let response = self.client.get_usage().await?;
        let snapshot = OpenCodeUsageSnapshot {
            response,
            fetched_at: OffsetDateTime::now_utc(),
        };
        *cache = Some(CacheEntry {
            snapshot: snapshot.clone(),
            stored_at: now,
        });
        Ok(snapshot)
    }
}

pub fn format_text(response: &OpenCodeUsageResponse) -> String {
    let rows = [
        ("Rolling (5 hour)", response.usage.rolling.as_ref()),
        ("Weekly", response.usage.weekly.as_ref()),
        ("Monthly", response.usage.monthly.as_ref()),
    ];
    let mut output = String::from("OpenCode Go usage:");
    for (label, window) in rows {
        output.push_str(&format!("\n  {label}: {}", format_window(window)));
    }
    output
}

fn format_window(window: Option<&OpenCodeUsageWindow>) -> String {
    let Some(window) = window else {
        return "unavailable".to_string();
    };

    let mut fields = Vec::new();
    if let Some(percent) = window.percent {
        fields.push(format!("{percent}% used"));
    }
    if let Some(status) = window.status.as_deref() {
        fields.push(format!("status {status}"));
    }
    if let Some(reset_at) = window.resets_at.as_deref() {
        fields.push(format!("resets {reset_at}"));
    }

    if fields.is_empty() {
        "unavailable".to_string()
    } else {
        fields.join(", ")
    }
}

pub fn ccr_snapshot(snapshot: &OpenCodeUsageSnapshot) -> Value {
    let mut meters = Vec::new();
    let mut clamped = false;

    for (id, label, window_name, window) in [
        (
            "rolling",
            "5h quota",
            "5h",
            snapshot.response.usage.rolling.as_ref(),
        ),
        (
            "weekly",
            "Weekly quota",
            "weekly",
            snapshot.response.usage.weekly.as_ref(),
        ),
        (
            "monthly",
            "Monthly quota",
            "monthly",
            snapshot.response.usage.monthly.as_ref(),
        ),
    ] {
        let Some(window) = window else {
            continue;
        };
        let (meter, meter_clamped) = ccr_meter(id, label, window_name, window);
        meters.push(meter);
        clamped |= meter_clamped;
    }

    let mut result = Map::new();
    result.insert("provider".into(), Value::String("OpenCode Go".into()));
    result.insert(
        "status".into(),
        Value::String(overall_status(&snapshot.response).into()),
    );
    result.insert("meters".into(), Value::Array(meters));
    result.insert(
        "updatedAt".into(),
        Value::String(
            snapshot
                .fetched_at
                .format(&Rfc3339)
                .expect("UTC timestamps are valid RFC 3339"),
        ),
    );
    if clamped {
        result.insert(
            "message".into(),
            Value::String(
                "OpenCode Go returned an out-of-range percentage; displayed values were clamped to 0-100%."
                    .into(),
            ),
        );
    }
    Value::Object(result)
}

fn ccr_meter(
    id: &str,
    label: &str,
    window_name: &str,
    window: &OpenCodeUsageWindow,
) -> (Value, bool) {
    let mut meter = Map::new();
    meter.insert("id".into(), Value::String(format!("opencode_go_{id}")));
    meter.insert("kind".into(), Value::String("quota".into()));
    meter.insert("label".into(), Value::String(label.into()));
    meter.insert("unit".into(), Value::String("%".into()));
    meter.insert("window".into(), Value::String(window_name.into()));

    let mut clamped = false;
    if let Some(percent) = window.percent {
        let normalized = percent.clamp(0.0, 100.0);
        clamped = normalized != percent;
        meter.insert("limit".into(), json!(100.0));
        meter.insert("used".into(), json!(normalized));
        meter.insert("remaining".into(), json!(100.0 - normalized));
    }
    if let Some(reset_at) = window.resets_at.as_deref() {
        meter.insert("resetAt".into(), Value::String(reset_at.into()));
    }
    if let Some(status) = window.status.as_deref() {
        meter.insert(
            "details".into(),
            json!([{"label": "OpenCode status", "status": status}]),
        );
    }

    (Value::Object(meter), clamped)
}

fn overall_status(response: &OpenCodeUsageResponse) -> &'static str {
    let windows = [
        response.usage.rolling.as_ref(),
        response.usage.weekly.as_ref(),
        response.usage.monthly.as_ref(),
    ];

    if windows.iter().flatten().any(|window| {
        window.status.as_deref() == Some("rate-limited")
            || window.percent.is_some_and(|percent| percent >= 95.0)
    }) {
        "critical"
    } else if windows
        .iter()
        .flatten()
        .any(|window| window.percent.is_some_and(|percent| percent >= 80.0))
    {
        "warning"
    } else {
        "ok"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Json, Router, response::IntoResponse, routing::get};

    use super::*;
    use crate::providers::opencode::client::OpenCodeUsage;

    fn window(
        status: Option<&str>,
        percent: Option<f64>,
        reset: Option<&str>,
    ) -> OpenCodeUsageWindow {
        OpenCodeUsageWindow {
            status: status.map(str::to_string),
            percent,
            resets_at: reset.map(str::to_string),
            extra: Default::default(),
        }
    }

    fn response() -> OpenCodeUsageResponse {
        OpenCodeUsageResponse {
            usage: OpenCodeUsage {
                rolling: Some(window(Some("ok"), Some(12.5), Some("2026-09-10T12:00:00Z"))),
                weekly: Some(window(Some("warning"), Some(82.0), None)),
                monthly: Some(window(Some("ok"), Some(33.0), Some("2026-10-01T00:00:00Z"))),
                extra: Default::default(),
            },
            extra: Default::default(),
        }
    }

    #[test]
    fn text_output_has_stable_window_order() {
        assert_eq!(
            format_text(&response()),
            concat!(
                "OpenCode Go usage:\n",
                "  Rolling (5 hour): 12.5% used, status ok, resets 2026-09-10T12:00:00Z\n",
                "  Weekly: 82% used, status warning\n",
                "  Monthly: 33% used, status ok, resets 2026-10-01T00:00:00Z"
            )
        );
    }

    #[test]
    fn text_output_marks_missing_windows_and_fields_unavailable() {
        let response = OpenCodeUsageResponse {
            usage: OpenCodeUsage {
                rolling: Some(window(None, None, None)),
                weekly: None,
                monthly: None,
                extra: Default::default(),
            },
            extra: Default::default(),
        };

        assert_eq!(
            format_text(&response),
            "OpenCode Go usage:\n  Rolling (5 hour): unavailable\n  Weekly: unavailable\n  Monthly: unavailable"
        );
    }

    #[test]
    fn ccr_response_contains_supported_meter_fields() {
        let snapshot = OpenCodeUsageSnapshot {
            response: response(),
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        };
        let value = ccr_snapshot(&snapshot);

        assert_eq!(value["provider"], "OpenCode Go");
        assert_eq!(value["status"], "warning");
        assert_eq!(value["updatedAt"], "1970-01-01T00:00:00Z");
        assert_eq!(value["meters"].as_array().unwrap().len(), 3);
        let rolling = &value["meters"][0];
        assert_eq!(rolling["id"], "opencode_go_rolling");
        assert_eq!(rolling["kind"], "quota");
        assert_eq!(rolling["used"], 12.5);
        assert_eq!(rolling["remaining"], 87.5);
        assert_eq!(rolling["limit"], 100.0);
        assert_eq!(rolling["unit"], "%");
        assert_eq!(rolling["window"], "5h");
        assert_eq!(rolling["resetAt"], "2026-09-10T12:00:00Z");
        assert_eq!(rolling["details"][0]["status"], "ok");
    }

    #[test]
    fn ccr_response_does_not_fabricate_missing_quota_values() {
        let snapshot = OpenCodeUsageSnapshot {
            response: OpenCodeUsageResponse {
                usage: OpenCodeUsage {
                    rolling: Some(window(Some("ok"), None, None)),
                    weekly: None,
                    monthly: None,
                    extra: Default::default(),
                },
                extra: Default::default(),
            },
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        };
        let value = ccr_snapshot(&snapshot);
        let meter = &value["meters"][0];

        assert_eq!(value["meters"].as_array().unwrap().len(), 1);
        assert!(meter.get("used").is_none());
        assert!(meter.get("remaining").is_none());
        assert!(meter.get("limit").is_none());
        assert!(meter.get("resetAt").is_none());
    }

    #[test]
    fn ccr_response_clamps_out_of_range_percentages_explicitly() {
        let mut response = response();
        response.usage.rolling.as_mut().unwrap().percent = Some(120.0);
        let snapshot = OpenCodeUsageSnapshot {
            response,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        };
        let value = ccr_snapshot(&snapshot);

        assert_eq!(value["meters"][0]["used"], 100.0);
        assert_eq!(value["meters"][0]["remaining"], 0.0);
        assert!(value["message"].as_str().unwrap().contains("clamped"));
    }

    #[test]
    fn rate_limited_window_is_critical() {
        let mut response = response();
        response.usage.rolling.as_mut().unwrap().status = Some("rate-limited".into());
        let snapshot = OpenCodeUsageSnapshot {
            response,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        };

        assert_eq!(ccr_snapshot(&snapshot)["status"], "critical");
    }

    #[tokio::test]
    async fn cache_reuses_success_until_ttl_and_then_refreshes() {
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_handler = Arc::clone(&requests);
        let app = Router::new().route(
            "/usage",
            get(move || {
                let requests = Arc::clone(&requests_for_handler);
                async move {
                    let count = requests.fetch_add(1, Ordering::SeqCst) + 1;
                    Json(json!({
                        "usage": {"rolling": {"percent": count as f64}}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client =
            OpenCodeClient::new(format!("http://{address}"), Some("secret".into())).unwrap();
        let ttl = Duration::from_secs(60);
        let service = OpenCodeUsageService::with_ttl(client, ttl);
        let start = Instant::now();

        let first = service.get_at(start).await.unwrap();
        let cached = service
            .get_at(start + Duration::from_secs(59))
            .await
            .unwrap();
        let refreshed = service.get_at(start + ttl).await.unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(first.response.usage.rolling.unwrap().percent, Some(1.0));
        assert_eq!(cached.response.usage.rolling.unwrap().percent, Some(1.0));
        assert_eq!(refreshed.response.usage.rolling.unwrap().percent, Some(2.0));
    }

    #[tokio::test]
    async fn expired_cache_does_not_hide_refresh_failure() {
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_handler = Arc::clone(&requests);
        let app = Router::new().route(
            "/usage",
            get(move || {
                let requests = Arc::clone(&requests_for_handler);
                async move {
                    if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                        Json(json!({"usage": {"rolling": {"percent": 10}}})).into_response()
                    } else {
                        (
                            http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error": {"message": "temporarily unavailable"}})),
                        )
                            .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client =
            OpenCodeClient::new(format!("http://{address}"), Some("secret".into())).unwrap();
        let ttl = Duration::from_secs(60);
        let service = OpenCodeUsageService::with_ttl(client, ttl);
        let start = Instant::now();

        service.get_at(start).await.unwrap();
        let error = service.get_at(start + ttl).await.unwrap_err();

        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(error.status, http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.message, "temporarily unavailable");
    }
}
