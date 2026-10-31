//! Exercise the real proxy process with synthetic auth and loopback HTTP/WebSocket upstreams.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Router, routing::post};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Default)]
struct Script {
    attempts: usize,
    fail: bool,
}

fn next_events(script: &Mutex<Script>) -> Vec<Value> {
    let mut script = script.lock().unwrap();
    script.attempts += 1;
    if script.fail {
        return vec![
            json!({"type":"response.failed","response":{"status":"failed","error":{
                "code":"invalid_prompt","message":"scripted rejection"
            }}}),
        ];
    }
    let mut events = vec![];
    if script.attempts >= 3 {
        events.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_recovered"}}));
        events.push(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"recovered"}));
        events.push(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_recovered"}}));
    }
    events.push(json!({"type":"response.completed","response":{
        "id":format!("resp_{}", script.attempts),"status":"completed","output":[],
        "usage":{"input_tokens":1,"output_tokens":0}
    }}));
    events
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_process_empty_tool_completion_preserves_negative_cases() {
    for transport in ["http", "websocket"] {
        let script = Arc::new(Mutex::new(Script::default()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let state = script.clone();
        let upstream_task = tokio::spawn(async move {
            if transport == "http" {
                let app = Router::new().fallback(post(move || {
                    let state = state.clone();
                    async move {
                        let body = next_events(&state)
                            .into_iter()
                            .map(|event| format!("data: {event}\n\n"))
                            .collect::<String>();
                        ([("content-type", "text/event-stream")], body)
                    }
                }));
                axum::serve(listener, app).await.unwrap();
            } else {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let state = state.clone();
                    tokio::spawn(async move {
                        let mut socket = accept_async(stream).await.unwrap();
                        while let Some(Ok(message)) = socket.next().await {
                            match message {
                                Message::Text(_) => {
                                    for event in next_events(&state) {
                                        if socket
                                            .send(Message::Text(event.to_string()))
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                                Message::Ping(payload) => {
                                    let _ = socket.send(Message::Pong(payload)).await;
                                }
                                Message::Close(_) => return,
                                _ => {}
                            }
                        }
                    });
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("codex")).unwrap();
        std::fs::write(
            dir.path().join("codex/auth.json"),
            json!({
                "access":"synthetic-access","refresh":"synthetic-refresh",
                "expires":4_102_444_800_000_i64,"account_id":"synthetic-account"
            })
            .to_string(),
        )
        .unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"));
        command
            .env_clear()
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
            .args(["serve", "--port", &port.to_string(), "--no-monitor"])
            .env("CCP_CONFIG_DIR", dir.path())
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("CCP_CODEX_TRANSPORT", transport)
            .env("CCP_CODEX_BASE_URL", format!("http://{upstream}"))
            .env("CCP_CODEX_PREVIOUS_RESPONSE_ID", "0")
            .env("CCP_CODEX_SERVER_COMPACTION", "0")
            .env("CCP_ALIAS_PROVIDER", "codex")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _proxy = Child(command.spawn().unwrap());
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let mut ready = false;
        for _ in 0..100 {
            if client.get(format!("{base}/healthz")).send().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(ready, "proxy did not start");
        for stream in [false, true] {
            for case in [
                "success",
                "whitespace",
                "text",
                "hook",
                "system",
                "partial",
                "unmatched",
                "error",
                "failed",
            ] {
                *script.lock().unwrap() = Script {
                    attempts: 0,
                    fail: case == "failed",
                };
                let mut results =
                    vec![json!({"type":"tool_result","tool_use_id":"a","content":"sent"})];
                match case {
                    "whitespace" => results.push(json!({"type":"text","text":" \n\t"})),
                    "text" => results.push(json!({"type":"text","text":"please continue"})),
                    "hook" => results.push(json!({"type":"text","text":"<system-reminder>PostToolUse hook: continue</system-reminder>"})),
                    "unmatched" => results[0]["tool_use_id"] = json!("unknown"),
                    "error" => results[0]["is_error"] = json!(true),
                    _ => {}
                }
                let mut calls = vec![json!({"type":"tool_use","id":"a","name":"reply","input":{}})];
                if case == "partial" {
                    calls.push(json!({"type":"tool_use","id":"b","name":"reply","input":{}}));
                }
                let mut messages = vec![
                    json!({"role":"user","content":"ping"}),
                    json!({"role":"assistant","content":calls}),
                    json!({"role":"user","content":results}),
                ];
                if case == "system" {
                    messages.push(json!({"role":"system","content":"PostToolUse hook: continue"}));
                }
                let response = client
                    .post(format!("{base}/v1/messages"))
                    .json(&json!({
                        "model":"gpt-5.6-sol","max_tokens":64,"stream":stream,"messages":messages,
                        "tools":[{"name":"reply","input_schema":{"type":"object","properties":{}}}]
                    }))
                    .send()
                    .await
                    .unwrap_or_else(|error| {
                        panic!("{transport} stream={stream} case={case}: {error}")
                    });
                let status = response.status();
                let body = response.text().await.unwrap();
                let attempts = script.lock().unwrap().attempts;
                let context = format!(
                    "{transport} stream={stream} case={case}: {status} attempts={attempts} body={body}"
                );
                if case == "failed" {
                    assert!(!status.is_success(), "{context}");
                    assert_eq!(attempts, 1, "{context}");
                } else {
                    assert!(status.is_success(), "{context}");
                    let eligible = matches!(case, "success" | "whitespace");
                    assert_eq!(attempts, if eligible { 2 } else { 3 }, "{context}");
                    assert_eq!(body.contains("recovered"), !eligible, "{context}");
                    if stream {
                        assert!(body.contains("event: message_stop"), "{context}");
                        assert!(body.contains("\"stop_reason\":\"end_turn\""), "{context}");
                    } else {
                        let message: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(message["stop_reason"], "end_turn", "{context}");
                        if eligible {
                            assert_eq!(message["content"], json!([]), "{context}");
                        }
                    }
                }
            }
        }
        upstream_task.abort();
    }
}
