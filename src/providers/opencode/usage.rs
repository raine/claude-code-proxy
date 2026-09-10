use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::client::{OpenCodeUsageResponse, OpenCodeUsageWindow};

pub fn format_text(response: &OpenCodeUsageResponse) -> String {
    let usage = &response.usage;
    let rows = [
        ("Rolling (5 hour)", &usage.rolling),
        ("Weekly", &usage.weekly),
        ("Monthly", &usage.monthly),
    ];
    let mut output = String::from("OpenCode Go usage:");
    for (label, window) in rows {
        output.push_str(&format!(
            "\n  {label}: {}% used, status {}, resets {}",
            window.percent, window.status, window.resets_at
        ));
    }
    output
}

pub fn ccr_snapshot(response: &OpenCodeUsageResponse) -> Value {
    let usage = &response.usage;
    let windows = [
        ("rolling", "5h quota", "5h", &usage.rolling),
        ("weekly", "Weekly quota", "weekly", &usage.weekly),
        ("monthly", "Monthly quota", "monthly", &usage.monthly),
    ];
    let status = ccr_status(windows.map(|(_, _, _, window)| window));
    let meters = windows.map(|(id, label, period, window)| {
        let used = window.percent.clamp(0.0, 100.0);
        json!({
            "id": format!("opencode_go_{id}"),
            "kind": "quota",
            "label": label,
            "limit": 100,
            "remaining": 100.0 - used,
            "resetAt": window.resets_at,
            "unit": "%",
            "used": used,
            "window": period
        })
    });
    json!({
        "provider": "OpenCode Go",
        "status": status,
        "updatedAt": OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .expect("UTC timestamps support RFC 3339 formatting"),
        "meters": meters
    })
}

fn ccr_status<'a>(windows: impl IntoIterator<Item = &'a OpenCodeUsageWindow>) -> &'static str {
    let mut status = "ok";
    for window in windows {
        let remaining = 100.0 - window.percent;
        if window.status == "rate-limited" || remaining <= 5.0 {
            return "critical";
        }
        if remaining <= 20.0 {
            status = "warning";
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::opencode::client::{OpenCodeUsage, OpenCodeUsageWindow};

    fn response(monthly_percent: f64, monthly_status: &str) -> OpenCodeUsageResponse {
        OpenCodeUsageResponse {
            usage: OpenCodeUsage {
                rolling: OpenCodeUsageWindow {
                    status: "ok".to_string(),
                    percent: 12.5,
                    resets_at: "rolling-reset".to_string(),
                },
                weekly: OpenCodeUsageWindow {
                    status: "ok".to_string(),
                    percent: 34.0,
                    resets_at: "weekly-reset".to_string(),
                },
                monthly: OpenCodeUsageWindow {
                    status: monthly_status.to_string(),
                    percent: monthly_percent,
                    resets_at: "monthly-reset".to_string(),
                },
            },
        }
    }

    #[test]
    fn text_includes_every_window_in_order() {
        assert_eq!(
            format_text(&response(100.0, "rate-limited")),
            concat!(
                "OpenCode Go usage:\n",
                "  Rolling (5 hour): 12.5% used, status ok, resets rolling-reset\n",
                "  Weekly: 34% used, status ok, resets weekly-reset\n",
                "  Monthly: 100% used, status rate-limited, resets monthly-reset"
            )
        );
    }

    #[test]
    fn ccr_snapshot_uses_standard_meter_schema() {
        let snapshot = ccr_snapshot(&response(85.0, "ok"));
        assert_eq!(snapshot["provider"], "OpenCode Go");
        assert_eq!(snapshot["status"], "warning");
        assert_eq!(snapshot["meters"][0]["id"], "opencode_go_rolling");
        assert_eq!(snapshot["meters"][0]["window"], "5h");
        assert_eq!(snapshot["meters"][1]["window"], "weekly");
        assert_eq!(snapshot["meters"][2]["used"], 85.0);
        assert_eq!(snapshot["meters"][2]["remaining"], 15.0);
        assert_eq!(snapshot["meters"][2]["resetAt"], "monthly-reset");
    }

    #[test]
    fn rate_limited_window_is_critical() {
        let snapshot = ccr_snapshot(&response(100.0, "rate-limited"));
        assert_eq!(snapshot["status"], "critical");
    }
}
