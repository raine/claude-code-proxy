use crate::provider::RequestContext;
use std::fs::{self, OpenOptions};
use std::io::Write;

pub(super) fn notify(ctx: &RequestContext, state: &str, attempt: u32, message: &str) {
    let record = serde_json::json!({"reqId":ctx.req_id,"state":state,"attempt":attempt,"maxAttempts":3,"message":message,"time":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d|d.as_secs()).unwrap_or(0)});
    crate::logging::create_logger("codex").warn("connection_recovery", record.as_object().cloned());
    let Some(id) = ctx.notification_id.as_deref().filter(|id| valid_id(id)) else {
        return;
    };
    let directory = crate::paths::state_dir().join("notifications");
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(directory.join(format!("{id}.jsonl"))) {
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_paths_cannot_escape_their_directory() {
        assert!(valid_id("1234-1780000000000"));
        for id in ["", "../secret", "/tmp/file", "a/b", "a\nb"] {
            assert!(!valid_id(id));
        }
    }
}
