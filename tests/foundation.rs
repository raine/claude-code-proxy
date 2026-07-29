use claude_code_proxy::anthropic::{
    encode_sse_event, parse_sse_events, parse_sse_events_with_stats, schema::MessagesRequest,
};
use claude_code_proxy::auth::{AuthStorage, InMemoryAuthStore};
use claude_code_proxy::config::load_config;
use claude_code_proxy::logging::{create_logger, flush, log_file, redact_value};
use claude_code_proxy::paths::{self, DirResolverEnv};
use claude_code_proxy::retry::{RETRY_INITIAL_DELAY_MS, RETRY_MAX_DELAY_MS, compute_backoff_delay};
use claude_code_proxy::traffic::{
    MAX_SSE_CAPTURE_BYTES, TrafficCaptureOptions, create_traffic_capture, redact_traffic,
    sanitize_path_part, traffic_capture_enabled_for_env,
};
use serde_json::Map;
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn messages_fixture_and_sse_parsing() {
    let raw = std::fs::read_to_string("tests/fixtures/anthropic-message.json").unwrap();
    let req: MessagesRequest = serde_json::from_str(&raw).unwrap();
    assert_eq!(req.model.as_deref(), Some("gpt-5.4[1m]"));
    assert!(req.extra.contains_key("extra_field"));

    let raw_sse = std::fs::read_to_string("tests/fixtures/sse-basic.txt").unwrap();
    let events = parse_sse_events(raw_sse.as_bytes());
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event, None);
    assert_eq!(events[0].data, "{\"chunk\":1}");
    assert_eq!(events[1].event.as_deref(), Some("update"));
    assert_eq!(events[1].data, "{\"text\":\"line1\"}\n{\"text\":\"line2\"}");
    assert_eq!(events[2].event.as_deref(), Some("final"));
    assert_eq!(events[2].data, "{\"done\":true}");

    let (events_with_stats, stats) = parse_sse_events_with_stats(raw_sse.as_bytes());
    assert_eq!(events_with_stats.len(), events.len());
    assert!(stats.event_count >= 2);

    let encoded = encode_sse_event(Some("message"), "{\"a\":1}\n{\"b\":2}");
    assert_eq!(
        encoded,
        b"event: message\ndata: {\"a\":1}\ndata: {\"b\":2}\n\n".to_vec()
    );
    let reparsed = parse_sse_events(&encoded);
    assert_eq!(reparsed[0].event.as_deref(), Some("message"));
    assert_eq!(reparsed[0].data, "{\"a\":1}\n{\"b\":2}");
    let first_line: serde_json::Value =
        serde_json::from_str(reparsed[0].data.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["a"], json!(1));
}

#[test]
fn path_resolvers_cover_platform_rules() {
    let mut env = HashMap::new();
    env.insert("CCP_CONFIG_DIR".to_string(), "/tmp/ccp-config".to_string());
    let deps = DirResolverEnv {
        platform: "macos".to_string(),
        home: "/home/u".into(),
        env: env.clone(),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "/tmp/ccp-config"
    );

    let deps = DirResolverEnv {
        platform: "macos".to_string(),
        home: "/home/u".into(),
        env: HashMap::from([("XDG_CONFIG_HOME".into(), "/x".into())]),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "/home/u/.config/claude-code-proxy"
    );

    let deps = DirResolverEnv {
        platform: "linux".to_string(),
        home: "/home/u".into(),
        env: HashMap::from([("XDG_CONFIG_HOME".into(), "/x".into())]),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "/x/claude-code-proxy"
    );

    let deps = DirResolverEnv {
        platform: "windows".to_string(),
        home: "C:/Users/u".into(),
        env: HashMap::from([("APPDATA".into(), "C:/Users/u/AppData/Roaming".into())]),
    };
    assert_eq!(
        paths::resolve_config_dir(&deps).to_string_lossy(),
        "C:/Users/u/AppData/Roaming/claude-code-proxy"
    );
    assert_eq!(
        paths::resolve_state_dir(&DirResolverEnv {
            platform: "windows".to_string(),
            home: "C:/Users/u".into(),
            env: HashMap::from([("LOCALAPPDATA".into(), "C:/Users/u/AppData/Local".into())]),
        })
        .to_string_lossy(),
        "C:/Users/u/AppData/Local/claude-code-proxy"
    );
}

#[test]
fn logging_redaction_and_truncation() {
    let short =
        json!({"authorization": "abc", "visible": "x", "nested": {"set-cookie": "c", "v": "y"}});
    let redacted = redact_value(short);
    assert_eq!(redacted["visible"], json!("x"));
    assert!(
        redacted["authorization"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );

    let long = "a".repeat(5000);
    let long_payload = redact_value(json!({"text": long}));
    let text = long_payload["text"].as_str().unwrap();
    assert!(text.contains("[1000 more]"));
}

#[test]
fn traffic_capture_helpers() {
    let mut env = HashMap::new();
    env.insert("CCP_TRAFFIC_LOG".into(), "1".into());
    assert!(traffic_capture_enabled_for_env(&env));
    assert!(!traffic_capture_enabled_for_env(&HashMap::new()));

    assert_eq!(sanitize_path_part("a*b@c/def"), "a_b_c_def");
    let path = sanitize_path_part(
        "/a\
/path",
    );
    assert!(path.chars().all(|ch| ch != '/'));

    let sample = json!({"access_token":"abc","nested":{"chatgpt-account-id":"x","value":1}});
    let redacted = redact_traffic(&sample);
    assert!(
        redacted["access_token"]
            .as_str()
            .unwrap()
            .contains("redacted")
    );
    assert_eq!(
        redact_traffic(&json!({"user_id":"person","note":"Bearer secret"}))["user_id"],
        "[redacted len=6]"
    );
    let secrets = json!({
        "access_token":"access-secret",
        "nested":[{"refresh_token":"refresh-secret","oauth_token":"oauth-secret"}],
        "identity":{"email":"person@example.test"},
        "user":{"message":"Bearer text is ordinary message content"}
    });
    let redacted = redact_traffic(&secrets).to_string();
    for secret in [
        "access-secret",
        "refresh-secret",
        "oauth-secret",
        "person@example.test",
    ] {
        assert!(!redacted.contains(secret));
    }
    assert!(redacted.contains("Bearer text is ordinary message content"));

    unsafe {
        env::set_var("CCP_TRAFFIC_LOG", "1");
    }
    let temp = TempDir::new().unwrap();
    let capture = create_traffic_capture(TrafficCaptureOptions {
        req_id: "req-1".into(),
        session_id: None,
        session_seq: Some(12),
        provider: Some("codex".into()),
        state_dir_override: Some(temp.path().to_path_buf()),
    })
    .expect("capture");

    capture.write_text("020-note", "hello");
    capture.write_json("030-req", &json!({"token":"abc"}));
    let event = json!({"type":"response.completed","refresh_token":"secret"});
    let mut stream_capture = capture.stream_capture();
    for _ in 0..(MAX_SSE_CAPTURE_BYTES / 100 + 2) {
        stream_capture.upstream_event(Some("response.completed"), &event);
    }
    stream_capture.finish(&capture, json!({"kind":"test"}));
    let transcript = std::fs::read_to_string(
        std::fs::read_dir(capture.root())
            .unwrap()
            .find_map(|entry| {
                let path = entry.ok()?.path();
                path.file_name()?
                    .to_string_lossy()
                    .ends_with("032-upstream-response-body.sse")
                    .then_some(path)
            })
            .unwrap(),
    )
    .unwrap();
    assert!(transcript.len() <= MAX_SSE_CAPTURE_BYTES);
    assert!(!transcript.contains("secret"));
    unsafe {
        env::remove_var("CCP_TRAFFIC_LOG");
    }
}

#[test]
fn retry_backoff_decisions() {
    let outcome = compute_backoff_delay(0, None);
    assert!(outcome.wait_ms >= RETRY_INITIAL_DELAY_MS / 2);
    assert!(outcome.wait_ms <= RETRY_MAX_DELAY_MS);

    let outcome_num = compute_backoff_delay(0, Some("5"));
    assert_eq!(outcome_num.wait_ms, 5000);

    let too_long = compute_backoff_delay(0, Some("120"));
    assert!(too_long.exceeds_budget);
}

#[tokio::test]
async fn auth_store_read_write_and_logout() {
    let store: InMemoryAuthStore<serde_json::Value> = InMemoryAuthStore::new();
    assert!(store.load().unwrap().is_none());
    store.save(json!({"token": "abc"})).unwrap();
    let loaded = store.load().unwrap().expect("value");
    assert_eq!(loaded["token"], json!("abc"));
    store.clear().unwrap();
    assert!(store.load().unwrap().is_none());

    let path = tempfile::tempdir().unwrap().path().join("auth.json");
    assert!(!path.exists());
}

#[test]
fn config_env_precedence_and_defaults() {
    let original = env::var("PORT").ok();
    unsafe {
        env::remove_var("PORT");
    }
    let cfg = load_config();
    assert_eq!(cfg.port, 18765);

    unsafe {
        env::set_var("PORT", "19999");
    }
    let cfg = load_config();
    assert_eq!(cfg.port, 19999);

    if let Some(v) = original {
        unsafe {
            env::set_var("PORT", v);
        }
    } else {
        unsafe {
            env::remove_var("PORT");
        }
    }
}

#[test]
fn logger_factory_uses_test_process_log() {
    let production_log = paths::state_dir().join("proxy.log");
    let test_log = log_file();
    assert_ne!(test_log, production_log);
    assert!(test_log.to_string_lossy().contains(".test-logs"));

    let logger = create_logger("server");
    let mut fields = Map::new();
    let marker = format!("foundation-logger-test-{}", std::process::id());
    fields.insert("marker".into(), json!(marker));
    logger.debug("ready", Some(fields));
    assert!(flush(Duration::from_secs(1)));

    let contents = std::fs::read_to_string(test_log).unwrap();
    assert!(contents.contains(&marker));
}

#[test]
fn logger_concurrent_writes_are_complete_jsonl_records() {
    const THREADS: usize = 12;
    const RECORDS_PER_THREAD: usize = 80;

    let marker = format!("foundation-concurrent-{}", std::process::id());
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut writers = Vec::new();
    for thread in 0..THREADS {
        let marker = marker.clone();
        let barrier = barrier.clone();
        writers.push(std::thread::spawn(move || {
            let logger = create_logger("concurrency-test");
            barrier.wait();
            for sequence in 0..RECORDS_PER_THREAD {
                let mut fields = Map::new();
                fields.insert("test_run".into(), json!(marker));
                fields.insert("thread".into(), json!(thread));
                fields.insert("sequence".into(), json!(sequence));
                fields.insert("payload".into(), json!("x".repeat(2_048)));
                logger.info("concurrent_record", Some(fields));
            }
        }));
    }
    for writer in writers {
        writer.join().unwrap();
    }
    assert!(flush(Duration::from_secs(2)));

    let contents = std::fs::read_to_string(log_file()).unwrap();
    let mut observed = std::collections::HashSet::new();
    for line in contents.lines() {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        if record
            .pointer("/fields/test_run")
            .and_then(|value| value.as_str())
            == Some(marker.as_str())
        {
            observed.insert((
                record.pointer("/fields/thread").unwrap().as_u64().unwrap(),
                record
                    .pointer("/fields/sequence")
                    .unwrap()
                    .as_u64()
                    .unwrap(),
            ));
        }
    }
    assert_eq!(observed.len(), THREADS * RECORDS_PER_THREAD);
}
