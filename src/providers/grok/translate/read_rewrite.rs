use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::Value;

const MAX_REWRITE_NOTES: usize = 4_096;
const READ_OFFSET_REWRITE_THRESHOLD: i64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOffsetRewrite {
    pub offset: i64,
    pub file_path: Option<String>,
}

#[derive(Debug, Default)]
struct RewriteStore {
    order: VecDeque<String>,
    entries: HashMap<String, ReadOffsetRewrite>,
}

static READ_OFFSET_REWRITES: Lazy<Mutex<RewriteStore>> =
    Lazy::new(|| Mutex::new(RewriteStore::default()));

pub fn sanitize_read_args(name: &str, args: &str, call_id: Option<&str>) -> String {
    if name != "Read" || args.is_empty() {
        return args.to_string();
    }

    let parsed: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return args.to_string(),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => return args.to_string(),
    };

    let mut sanitized = obj.clone();
    let mut changed = false;

    let has_empty_pages = obj
        .get("pages")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.is_empty());
    if has_empty_pages {
        sanitized.remove("pages");
        changed = true;
    }

    if let Some(offset) = obj.get("offset").and_then(|v| v.as_i64())
        && offset >= READ_OFFSET_REWRITE_THRESHOLD
    {
        sanitized.remove("offset");
        changed = true;
        if let Some(call_id) = call_id.filter(|id| !id.is_empty()) {
            record_read_offset_rewrite(
                call_id,
                ReadOffsetRewrite {
                    offset,
                    file_path: obj
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                },
            );
        }
    }

    if changed {
        serde_json::to_string(&sanitized).unwrap_or_else(|_| args.to_string())
    } else {
        args.to_string()
    }
}

pub fn read_offset_rewrite(call_id: &str) -> Option<ReadOffsetRewrite> {
    READ_OFFSET_REWRITES
        .lock()
        .ok()
        .and_then(|store| store.entries.get(call_id).cloned())
}

fn record_read_offset_rewrite(call_id: &str, note: ReadOffsetRewrite) {
    let Ok(mut store) = READ_OFFSET_REWRITES.lock() else {
        return;
    };

    if !store.entries.contains_key(call_id) {
        store.order.push_back(call_id.to_string());
    }
    store.entries.insert(call_id.to_string(), note);

    while store.entries.len() > MAX_REWRITE_NOTES {
        let Some(oldest) = store.order.pop_front() else {
            break;
        };
        store.entries.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_read_args_removes_empty_pages() {
        let args = r#"{"file_path":"/tmp/a","pages":""}"#;
        let sanitized = sanitize_read_args("Read", args, None);
        let parsed: Value = serde_json::from_str(&sanitized).unwrap();
        assert!(parsed.get("pages").is_none());
        assert_eq!(
            parsed.get("file_path").and_then(|v| v.as_str()),
            Some("/tmp/a")
        );
    }

    #[test]
    fn sanitize_read_args_drops_and_records_absurd_offset() {
        let args = r#"{"file_path":"/tmp/a","offset":1300000,"limit":20}"#;
        let sanitized = sanitize_read_args("Read", args, Some("call_rewrite_test"));
        let parsed: Value = serde_json::from_str(&sanitized).unwrap();
        assert!(parsed.get("offset").is_none());
        assert_eq!(parsed.get("limit").and_then(|v| v.as_i64()), Some(20));

        let note = read_offset_rewrite("call_rewrite_test").unwrap();
        assert_eq!(note.offset, 1_300_000);
        assert_eq!(note.file_path.as_deref(), Some("/tmp/a"));
    }

    #[test]
    fn sanitize_read_args_keeps_normal_offset() {
        let args = r#"{"file_path":"/tmp/a","offset":1300,"limit":20}"#;
        let sanitized = sanitize_read_args("Read", args, Some("call_keep_test"));
        let parsed: Value = serde_json::from_str(&sanitized).unwrap();
        assert_eq!(parsed.get("offset").and_then(|v| v.as_i64()), Some(1_300));
        assert!(read_offset_rewrite("call_keep_test").is_none());
    }
}
