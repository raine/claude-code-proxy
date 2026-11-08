//! Tests written against the frozen independent Python vectors before the Rust codec.
use super::antigravity::replay;
use super::antigravity::{
    auth::{Credential, Store},
    config::Config,
};
use super::antigravity::{request::prepare, schema::ToolSet, transport::Reducer};
use serde_json::{Value, json};

#[test]
fn antigravity_endpoint_validation_disallows_credential_redirect_targets() {
    for value in [
        "http://example.com",
        "https://user:secret@example.com",
        "https://example.com/?token=x",
        "https://example.com/#x",
    ] {
        assert!(Config::validate_endpoint(value, false).is_err());
    }
    assert!(Config::validate_endpoint("https://cloudcode-pa.googleapis.com", false).is_ok());
    assert!(Config::validate_endpoint("http://127.0.0.1:1234", false).is_err());
    assert!(Config::validate_endpoint("http://127.0.0.1:1234", true).is_ok());
}

#[tokio::test]
async fn antigravity_auth_storage_is_strict_private_and_lock_inode_is_stable() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("antigravity/auth.json"));
    assert!(store.load().unwrap().is_none());
    assert!(
        !directory.path().join("antigravity").exists(),
        "inspection must not create directories"
    );
    let lock = store.lock(std::time::Duration::from_secs(1)).await.unwrap();
    let credential = Credential {
        profile_id: "profile".into(),
        project_id: Some("project".into()),
        access_token: "synthetic-access".into(),
        refresh_token: Some("synthetic-refresh".into()),
        expires_at: u64::MAX,
    };
    store.save(&lock, &credential).unwrap();
    assert_eq!(store.load().unwrap().unwrap().profile_id, "profile");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let inode = std::fs::metadata(directory.path().join("antigravity/auth.lock"))
            .unwrap()
            .ino();
        assert_eq!(
            std::fs::metadata(directory.path().join("antigravity/auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.clear(&lock).unwrap();
        assert_eq!(
            std::fs::metadata(directory.path().join("antigravity/auth.lock"))
                .unwrap()
                .ino(),
            inode
        );
    }
    drop(lock);
    let corrupt_path = directory.path().join("antigravity/auth.json");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options
        .open(&corrupt_path)
        .unwrap()
        .write_all(b"{bad private data")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&corrupt_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let error = store.load().err().unwrap();
    assert_eq!(error, "invalid");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&corrupt_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.load().err().unwrap(), "unreadable");
    }
}

#[tokio::test]
async fn antigravity_auth_lock_is_bounded_and_logout_invalidates_pending_login() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(directory.path().join("auth.json"));
    let mut lock = store.lock(std::time::Duration::from_secs(1)).await.unwrap();
    let original = lock.epoch().unwrap();
    assert!(
        store
            .lock(std::time::Duration::from_millis(20))
            .await
            .is_err()
    );
    store.clear(&lock).unwrap();
    lock.advance().unwrap();
    assert_ne!(lock.epoch().unwrap(), original);
    drop(lock);
    assert!(store.lock(std::time::Duration::from_secs(1)).await.is_ok());
}

fn binding() -> replay::Binding {
    replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "gemini-4-argon-low".into(),
    }
}

fn request_value() -> Value {
    json!({"model":"antigravity/gemini-4-argon-low", "max_tokens":100,
        "messages":[{"role":"user", "content":"hello"}]})
}

fn execution_id(index: u64) -> String {
    format!("aaaaaaaa-aaaa-4aaa-8aaa-{index:012x}")
}

fn default_lineage() -> replay::Lineage {
    replay::Lineage::new(
        "11111111-1111-4111-8111-111111111111".into(),
        "22222222-2222-4222-8222-222222222222".into(),
    )
    .unwrap()
}

fn state_at(lineage: &replay::Lineage, index: u64) -> replay::State {
    replay::State {
        lineage: lineage.clone(),
        native_checkpoint_index: index,
        user_execution: replay::UserExecution {
            active_id: execution_id(index),
            prior_recorded_id: index.checked_sub(1).map(execution_id),
        },
    }
}

fn encode_turn(
    contents: &Value,
    binding: &replay::Binding,
    lineage: &replay::Lineage,
    index: u64,
    display: &str,
    mapping: &Value,
) -> replay::Result<Value> {
    let stop = if mapping.as_array().is_some_and(|m| !m.is_empty()) {
        "tool_use"
    } else {
        "end_turn"
    };
    replay::encode(
        contents,
        binding,
        &state_at(lineage, index),
        stop,
        display,
        mapping,
    )
}

fn encode_default(
    contents: &Value,
    binding: &replay::Binding,
    display: &str,
    mapping: &Value,
) -> replay::Result<Value> {
    encode_turn(contents, binding, &default_lineage(), 0, display, mapping)
}

#[test]
fn antigravity_first_checkpoint_is_bounded_and_scope_bound() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let lineage = default_lineage();
    let other_lineage = replay::Lineage::new(
        "33333333-3333-4333-8333-333333333333".into(),
        "44444444-4444-4444-8444-444444444444".into(),
    )
    .unwrap();
    let contents = json!([{"role":"model","parts":[
        {"text":"private thought","thought":true,"thoughtSignature":"opaque-original"},
        {"functionCall":{"name":"native_name","args":{"payload":"{\"x\":17}"}},"thoughtSignature":"opaque-call"},
        {"text":"complete"}
    ]}]);
    let recipes =
        json!([{"local_id":"toolu_fixed","caller_name":"caller_name","codec_paths":["/payload"]}]);
    let mapping = replay::mapping(&contents, Some(&recipes)).unwrap();
    for display in ["full", "omitted"] {
        let other =
            encode_turn(&contents, &binding(), &other_lineage, 0, display, &mapping).unwrap();
        let blocks = encode_turn(&contents, &binding(), &lineage, 0, display, &mapping).unwrap();
        assert_eq!(
            replay::projection_values(&other).unwrap(),
            replay::projection_values(&blocks).unwrap()
        );
        assert!(
            blocks[0]["signature"]
                .as_str()
                .unwrap()
                .starts_with(replay::PREFIX)
        );
        let envelope = replay::decode(&blocks, &binding()).unwrap();
        assert_eq!(envelope["contents"], contents);
        assert_eq!(envelope["tool_mapping"], mapping);
        assert_eq!(envelope["version"], 1);
        assert_eq!(envelope["backend_session_id"], "-3750763034362895579");
        assert_eq!(
            envelope["lineage"]["conversation_id"],
            lineage.conversation_id
        );
        assert_eq!(envelope["lineage"]["trajectory_id"], lineage.trajectory_id);
        assert_ne!(
            envelope["projection_digest"],
            replay::decode(&other, &binding()).unwrap()["projection_digest"]
        );
        for key in ["profile_id", "project_id", "native_model"] {
            let mut scope = serde_json::to_value(binding()).unwrap();
            scope[key] = json!("different");
            let scope = serde_json::from_value(scope).unwrap();
            assert!(replay::decode(&blocks, &scope).is_err());
        }
        for (key, value) in [
            ("version", json!(2)),
            ("version", json!(3)),
            ("backend_session_id", json!("19")),
            ("native_checkpoint_index", json!(1)),
            ("native_checkpoint_index", json!(2)),
            (
                "lineage",
                json!({"conversation_id":lineage.conversation_id,"trajectory_id":lineage.conversation_id}),
            ),
            (
                "lineage",
                json!({"conversation_id":"33333333-3333-4333-8333-333333333333","trajectory_id":lineage.trajectory_id}),
            ),
            (
                "projection_digest",
                replay::decode(&other, &binding()).unwrap()["projection_digest"].clone(),
            ),
            ("extra", json!(true)),
        ] {
            let mut tampered = envelope.clone();
            tampered[key] = value;
            let mut candidate = blocks.clone();
            candidate[0]["signature"] = json!(format!(
                "{}{}",
                replay::PREFIX,
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&tampered).unwrap())
            ));
            assert!(
                replay::decode(&candidate, &binding()).is_err(),
                "accepted {key}"
            );
        }
        let mut modified = blocks.clone();
        modified[1]["input"]["payload"]["x"] = json!(18);
        assert!(replay::decode(&modified, &binding()).is_err());
        let mut foreign = blocks.clone();
        foreign[0]["signature"] = json!("ccp:antigravity:v3:unknown");
        assert!(replay::decode(&foreign, &binding()).is_err());
        foreign[0]["signature"] = json!(format!("{}!", replay::PREFIX));
        assert!(replay::decode(&foreign, &binding()).is_err());
    }
    for id in [
        "not-a-uuid",
        "11111111111141118111111111111111",
        "11111111-1111-1111-8111-111111111111",
        "11111111-1111-4111-0111-111111111111",
        "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
    ] {
        assert!(replay::Lineage::new(id.into(), lineage.trajectory_id.clone()).is_err());
    }
    let oversized = json!([{"role":"model","parts":[{"text":"x".repeat(replay::MAX_DECODED)}]}]);
    assert_eq!(
        encode_turn(&oversized, &binding(), &lineage, 0, "full", &json!([])).err(),
        Some("JSON byte limit")
    );
    let too_many = json!(vec![json!({"role":"model","parts":[]}); 513]);
    assert!(encode_turn(&too_many, &binding(), &lineage, 0, "full", &json!([])).is_err());
    assert!(
        replay::Lineage::new(lineage.conversation_id.clone(), lineage.conversation_id).is_err()
    );
}

#[test]
fn antigravity_metadata_first_request_and_history_continuation() {
    use super::antigravity::request::Metadata;
    let body = serde_json::from_value(request_value()).unwrap();
    let mut fresh = prepare(&body, &binding()).unwrap();
    assert!(
        matches!(fresh.metadata, Metadata::Fresh),
        "preparation must not allocate Go identifiers"
    );
    assert_eq!(fresh.wire["requestId"], "");
    assert_eq!(fresh.wire["request"]["sessionId"], "");
    let lineage = default_lineage();
    assert!(
        fresh
            .finalize_metadata(-1, || panic!("invalid clock must not allocate"))
            .is_err()
    );
    let mut allocated = 0;
    let mut ids = [
        lineage.conversation_id.clone(),
        lineage.trajectory_id.clone(),
        execution_id(0),
    ]
    .into_iter();
    fresh
        .finalize_metadata(1_799_999_999_123, || {
            allocated += 1;
            ids.next().unwrap()
        })
        .unwrap();
    assert_eq!(
        allocated, 3,
        "two logical IDs and one proxy user execution at admission"
    );
    assert_eq!(
        fresh.wire["requestId"],
        "agent/11111111-1111-4111-8111-111111111111/1799999999123/22222222-2222-4222-8222-222222222222/1"
    );
    assert_eq!(fresh.wire["request"]["sessionId"], "-3750763034362895579");
    assert_eq!(
        fresh.wire["request"]["labels"],
        json!({
            "model_enum":"MODEL_PLACEHOLDER_M419","used_claude":"false","used_claude_conservative":"false",
            "used_non_gemini_model":"false","last_step_index":"0","cascade_id":lineage.conversation_id,
            "root_cascade_id":lineage.conversation_id,"trajectory_id":lineage.trajectory_id,
            "request_id":format!("{}-0", lineage.trajectory_id)
        })
    );
    assert!(
        fresh
            .finalize_metadata(1_799_999_999_123, || panic!(
                "double finalization must not allocate"
            ))
            .is_err()
    );
    let contents = json!([{"role":"model","parts":[{"text":"previous"}]}]);
    let turn = encode_turn(&contents, &binding(), &lineage, 0, "full", &json!([])).unwrap();
    let mut history = request_value();
    history["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":turn}),
        json!({"role":"user","content":"next"}),
    ]);
    // Ignored noncarrier signatures on text blocks must not invalidate a valid signed turn.
    history["messages"][1]["content"][1]["signature"] =
        json!("ccp:antigravity:v1:ignored-text-signature");
    let resumed = prepare(&serde_json::from_value(history).unwrap(), &binding()).unwrap();
    assert!(matches!(
        resumed.metadata,
        Metadata::Continuation {
            new_user_execution: true,
            ..
        }
    ));
}

#[test]
fn antigravity_two_user_text_checkpoint_progress_and_history_consistency() {
    use super::antigravity::request::Metadata;
    let lineage = default_lineage();
    let first_contents =
        json!([{"role":"model","parts":[{"text":"first","thoughtSignature":"original-opaque"}]}]);
    let second_contents = json!([{"role":"model","parts":[{"text":"second"}]}]);
    let first = encode_turn(&first_contents, &binding(), &lineage, 0, "full", &json!([])).unwrap();
    let second = encode_turn(
        &second_contents,
        &binding(),
        &lineage,
        1,
        "full",
        &json!([]),
    )
    .unwrap();
    let decoded = replay::decode_history(&json!([first, second]), &binding()).unwrap();
    assert_eq!(decoded[0]["native_checkpoint_index"], 0);
    assert_eq!(decoded[1]["native_checkpoint_index"], 1);
    assert_eq!(decoded[0]["contents"], first_contents);
    assert!(
        encode_turn(
            &second_contents,
            &binding(),
            &lineage,
            512,
            "full",
            &json!([])
        )
        .is_err()
    );
    for history in [
        json!([second]),
        json!([second, first]),
        json!([first, first]),
    ] {
        assert!(
            replay::decode_history(&history, &binding()).is_err(),
            "checkpoint omission/reorder/duplicate"
        );
    }
    let foreign = replay::Lineage::new(
        "33333333-3333-4333-8333-333333333333".into(),
        "44444444-4444-4444-8444-444444444444".into(),
    )
    .unwrap();
    let other_second = encode_turn(
        &second_contents,
        &binding(),
        &foreign,
        1,
        "full",
        &json!([]),
    )
    .unwrap();
    assert!(replay::decode_history(&json!([first, other_second]), &binding()).is_err());
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first}),
        json!({"role":"user","content":"next"}),
    ]);
    let mut prepared = prepare(&serde_json::from_value(body.clone()).unwrap(), &binding()).unwrap();
    let Metadata::Continuation {
        state,
        new_user_execution: true,
        ..
    } = &prepared.metadata
    else {
        panic!("expected carried lineage, no fresh allocation yet")
    };
    assert_eq!(state.lineage.conversation_id, lineage.conversation_id);
    assert_eq!(state.lineage.trajectory_id, lineage.trajectory_id);
    assert_eq!(prepared.wire["requestId"], "");
    assert_eq!(prepared.wire["request"]["sessionId"], "");
    assert!(
        prepared
            .finalize_metadata(1_799_999_999_456, || lineage.conversation_id.clone())
            .is_err(),
        "reject detected user-execution/lineage collision without reminting"
    );
    let mut allocations = 0;
    prepared
        .finalize_metadata(1_799_999_999_456, || {
            allocations += 1;
            execution_id(1)
        })
        .unwrap();
    assert_eq!(
        allocations, 1,
        "ordinary new user allocates only its active execution"
    );
    assert!(
        matches!(prepared.metadata, Metadata::Ready(ref state) if state.native_checkpoint_index == 1)
    );
    assert_eq!(
        prepared.wire["request"]["labels"]["last_execution_id"],
        execution_id(0)
    );
    assert_eq!(
        prepared.wire["request"]["labels"]["request_id"],
        format!("{}-1", lineage.trajectory_id)
    );
    assert_eq!(prepared.wire["request"]["labels"]["last_step_index"], "2");
    assert_eq!(
        prepared.wire["requestId"],
        format!(
            "agent/{}/1799999999456/{}/3",
            lineage.conversation_id, lineage.trajectory_id
        )
    );
    assert_eq!(
        prepared.wire["request"]["contents"],
        json!([
            {"role":"user","parts":[{"text":"hello"}]}, first_contents[0], {"role":"user","parts":[{"text":"next"}]}
        ])
    );
    // A carried native assistant checkpoint counts once; no hidden Go operation
    // counters are inferred from the number of native SSE fragments/parts.
    let fragmented = json!([
        {"role":"model","parts":[{"text":"part one","thoughtSignature":"opaque-one"}]},
        {"role":"model","parts":[{"text":"part two","thoughtSignature":"opaque-two"}]}
    ]);
    let fragmented_blocks =
        encode_turn(&fragmented, &binding(), &lineage, 0, "full", &json!([])).unwrap();
    let mut split_body = body.clone();
    split_body["messages"][1]["content"] = fragmented_blocks;
    let mut split = prepare(&serde_json::from_value(split_body).unwrap(), &binding()).unwrap();
    split
        .finalize_metadata(1_799_999_999_456, || execution_id(1))
        .unwrap();
    assert_eq!(split.wire["request"]["labels"]["last_step_index"], "2");
    assert_eq!(split.wire["request"]["contents"][1], fragmented[0]);
    assert_eq!(split.wire["request"]["contents"][2], fragmented[1]);
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":second}),
        json!({"role":"user","content":"unobserved third user"}),
    ]);
    let mut third = prepare(&serde_json::from_value(body).unwrap(), &binding()).unwrap();
    assert!(
        third
            .finalize_metadata(1_799_999_999_789, || execution_id(0))
            .is_err(),
        "new USER execution must not reuse an older carried identity"
    );
    third
        .finalize_metadata(1_799_999_999_789, || execution_id(2))
        .unwrap();
    assert_eq!(
        third.wire["request"]["labels"]["request_id"],
        format!("{}-2", lineage.trajectory_id)
    );
    assert_eq!(third.wire["request"]["labels"]["last_step_index"], "4");
    assert_eq!(
        third.wire["request"]["labels"]["last_execution_id"],
        execution_id(1)
    );
    assert!(third.wire["requestId"].as_str().unwrap().ends_with("/5"));
}

#[test]
fn antigravity_unqualified_tool_and_noncanonical_text_transitions_reject() {
    let lineage = default_lineage();
    let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"tool","args":{}}}]}]);
    let mapping = replay::mapping(&contents, None).unwrap();
    let tool_turn = encode_turn(&contents, &binding(), &lineage, 0, "full", &mapping).unwrap();
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":tool_turn}),
        json!({"role":"user","content":"ordinary text without tool_result"}),
    ]);
    assert!(prepare(&serde_json::from_value(body).unwrap(), &binding(),).is_err());
    let text = encode_turn(
        &json!([{"role":"model","parts":[{"text":"first"}]}]),
        &binding(),
        &lineage,
        0,
        "full",
        &json!([]),
    )
    .unwrap();
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":text}),
        json!({"role":"user","content":"next"}),
    ]);
    for signature in [
        "ccp:antigravity:v1:!",
        "ccp:antigravity:v2:unknown",
        "ccp:antigravity:v3:unknown",
        "foreign",
    ] {
        let mut invalid = body.clone();
        invalid["messages"][1]["content"][0]["signature"] = json!(signature);
        assert!(prepare(&serde_json::from_value(invalid).unwrap(), &binding(),).is_err());
    }
    let mut changed_scope = binding();
    changed_scope.profile_id = "other".into();
    assert!(
        prepare(
            &serde_json::from_value(body.clone()).unwrap(),
            &changed_scope,
        )
        .is_err()
    );
    body["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"additional unobserved role"}));
    assert!(prepare(&serde_json::from_value(body).unwrap(), &binding(),).is_err());
}

#[test]
fn antigravity_user_execution_survives_read_loop_then_records_for_next_user() {
    use super::antigravity::request::Metadata;
    let lineage = default_lineage();
    let initial = state_at(&lineage, 0);
    let call = json!([{"role":"model","parts":[{"functionCall":{"name":"read_file","args":{"Path":"/synthetic/owned"},"id":"synthetic-read-call-1"},"thoughtSignature":"c3ludGhldGljLXNpZ25hdHVyZQ=="}]}]);
    let mapping = replay::mapping(&call, None).unwrap();
    let first = replay::encode(&call, &binding(), &initial, "tool_use", "full", &mapping).unwrap();
    let saved = replay::decode(&first, &binding()).unwrap();
    assert_eq!(saved["checkpoint_stop_reason"], "tool_use");
    assert!(saved["user_execution"]["prior_recorded_id"].is_null());
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":first}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":first[1]["id"],"content":"read marker"}]})
    ]);
    let mut tool = prepare(&serde_json::from_value(body.clone()).unwrap(), &binding()).unwrap();
    assert!(matches!(
        tool.metadata,
        Metadata::Continuation {
            new_user_execution: false,
            ..
        }
    ));
    tool.finalize_metadata(1_799_999_999_456, || {
        panic!("tool loop must not mint any IDs")
    })
    .unwrap();
    let Metadata::Ready(ref running) = tool.metadata else {
        panic!("metadata ready")
    };
    assert_eq!(running.native_checkpoint_index, 1);
    assert_eq!(running.user_execution.active_id, execution_id(0));
    assert_eq!(running.user_execution.prior_recorded_id, None);
    assert!(
        tool.wire["request"]["labels"]
            .get("last_execution_id")
            .is_none()
    );
    assert_eq!(tool.wire["request"]["labels"]["last_step_index"], "2");
    assert_eq!(tool.wire["request"]["contents"][1], call[0]);
    assert_eq!(
        tool.wire["request"]["contents"][2],
        json!({"role":"model","parts":[{"functionResponse":{"name":"read_file","response":{"output":"read marker"},"id":"synthetic-read-call-1"}}]})
    );
    let completion = json!([{"role":"model","parts":[{"text":"done"}]}]);
    let second = replay::encode(
        &completion,
        &binding(),
        running,
        "end_turn",
        "full",
        &json!([]),
    )
    .unwrap();
    let decoded = replay::decode(&second, &binding()).unwrap();
    let complete = replay::State::from_envelope(&decoded).unwrap();
    assert_eq!(
        complete.recorded_after("end_turn").unwrap(),
        Some(execution_id(0).as_str())
    );
    assert_eq!(
        complete.user_execution.active_id,
        initial.user_execution.active_id
    );
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":second}),
        json!({"role":"user","content":"next ordinary user"}),
    ]);
    let mut next = prepare(&serde_json::from_value(body).unwrap(), &binding()).unwrap();
    let mut calls = 0;
    next.finalize_metadata(1_799_999_999_789, || {
        calls += 1;
        execution_id(1)
    })
    .unwrap();
    assert_eq!(calls, 1);
    let Metadata::Ready(ref next_state) = next.metadata else {
        panic!("ready")
    };
    assert_eq!(
        next_state.native_checkpoint_index, 2,
        "generation ordinal is separate from USER execution identity"
    );
    assert_eq!(next_state.user_execution.active_id, execution_id(1));
    assert_eq!(
        next_state.user_execution.prior_recorded_id,
        Some(execution_id(0))
    );
    assert_eq!(
        next.wire["request"]["labels"]["last_execution_id"],
        execution_id(0)
    );
    assert_eq!(
        next.wire["request"]["labels"]["request_id"],
        format!("{}-2", lineage.trajectory_id)
    );
    let third = replay::encode(
        &completion,
        &binding(),
        next_state,
        "end_turn",
        "full",
        &json!([]),
    )
    .unwrap();
    assert!(replay::decode_history(&json!([first, second, third]), &binding()).is_ok());
}

#[test]
fn antigravity_incomplete_or_invalid_terminal_state_never_records_success() {
    let lineage = default_lineage();
    let state = state_at(&lineage, 0);
    let text = json!([{"role":"model","parts":[{"text":"limited"}]}]);
    let limited =
        replay::encode(&text, &binding(), &state, "max_tokens", "full", &json!([])).unwrap();
    let saved = replay::decode(&limited, &binding()).unwrap();
    let incomplete = replay::State::from_envelope(&saved).unwrap();
    assert_eq!(incomplete.recorded_after("max_tokens").unwrap(), None);
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":limited}),
        json!({"role":"user","content":"not a proven continuation"}),
    ]);
    assert!(prepare(&serde_json::from_value(body).unwrap(), &binding(),).is_err());
    assert!(replay::encode(&text, &binding(), &state, "tool_use", "full", &json!([])).is_err());
    for stop in ["STOP", "", "error", "cancelled"] {
        assert!(replay::encode(&text, &binding(), &state, stop, "full", &json!([])).is_err());
    }
    let call = json!([{"role":"model","parts":[{"functionCall":{"name":"tool","id":"native","args":{}}}]}]);
    let map = replay::mapping(&call, None).unwrap();
    assert!(replay::encode(&call, &binding(), &state, "end_turn", "full", &map).is_err());
    for id in [
        lineage.conversation_id.clone(),
        lineage.trajectory_id.clone(),
        "bad".into(),
    ] {
        let mut invalid = state.clone();
        invalid.user_execution.active_id = id;
        assert!(
            replay::encode(&text, &binding(), &invalid, "end_turn", "full", &json!([])).is_err()
        );
    }
    let mut invalid = state;
    invalid.user_execution.prior_recorded_id = Some(execution_id(7));
    assert!(
        replay::encode(&text, &binding(), &invalid, "end_turn", "full", &json!([])).is_err(),
        "first checkpoint has no previous execution"
    );
}

#[test]
fn antigravity_execution_predecessors_and_termination_are_digest_bound() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let lineage = default_lineage();
    let text = json!([{"role":"model","parts":[{"text":"complete"}]}]);
    let first = encode_turn(&text, &binding(), &lineage, 0, "full", &json!([])).unwrap();
    let second = encode_turn(&text, &binding(), &lineage, 1, "full", &json!([])).unwrap();
    let envelope = replay::decode(&first, &binding()).unwrap();
    for (key, value) in [
        (
            "user_execution",
            json!({"active_id":execution_id(7),"prior_recorded_id":null}),
        ),
        (
            "user_execution",
            json!({"active_id":execution_id(0),"prior_recorded_id":execution_id(9)}),
        ),
        ("user_execution", json!({"active_id":execution_id(0)})),
        (
            "user_execution",
            json!({"active_id":execution_id(0),"prior_recorded_id":null,"extra":true}),
        ),
        ("checkpoint_stop_reason", json!("max_tokens")),
    ] {
        let mut modified = envelope.clone();
        modified[key] = value;
        let mut blocks = first.clone();
        blocks[0]["signature"] = json!(format!(
            "{}{}",
            replay::PREFIX,
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&modified).unwrap())
        ));
        assert!(replay::decode(&blocks, &binding()).is_err());
    }
    let mut wrong_predecessor = state_at(&lineage, 1);
    wrong_predecessor.user_execution.prior_recorded_id = Some(execution_id(7));
    let wrong = replay::encode(
        &text,
        &binding(),
        &wrong_predecessor,
        "end_turn",
        "full",
        &json!([]),
    )
    .unwrap();
    assert!(replay::decode_history(&json!([first, wrong]), &binding()).is_err());
    let mut reused = state_at(&lineage, 2);
    reused.user_execution.active_id = execution_id(0);
    let wrong = replay::encode(&text, &binding(), &reused, "end_turn", "full", &json!([])).unwrap();
    assert!(replay::decode_history(&json!([first, second, wrong]), &binding()).is_err());
    let call = json!([{"role":"model","parts":[{"functionCall":{"name":"read_file","id":"native","args":{}},"thoughtSignature":"opaque"}]}]);
    let mapping = replay::mapping(&call, None).unwrap();
    let tool_first = replay::encode(
        &call,
        &binding(),
        &state_at(&lineage, 0),
        "tool_use",
        "full",
        &mapping,
    )
    .unwrap();
    let mut reset = state_at(&lineage, 1);
    reset.user_execution.prior_recorded_id = None;
    let changed_active =
        replay::encode(&text, &binding(), &reset, "end_turn", "full", &json!([])).unwrap();
    assert!(
        replay::decode_history(&json!([tool_first, changed_active]), &binding()).is_err(),
        "tool continuation cannot reset active execution"
    );
}

fn metadata_count_context() -> crate::provider::RequestContext {
    crate::provider::RequestContext {
        req_id: "synthetic-pure-count".into(),
        session_id: None,
        session_seq: None,
        provider: "antigravity".into(),
        traffic: None,
        monitor: None,
    }
}

async fn counting_provider(
    directory: &std::path::Path,
    url: &url::Url,
    project: Option<&str>,
    expires: u64,
) -> (super::antigravity::AntigravityProvider, Store) {
    let store = Store::new(directory.join("antigravity/auth.json"));
    let lock = store.lock(std::time::Duration::from_secs(1)).await.unwrap();
    store
        .save(
            &lock,
            &Credential {
                profile_id: "profile".into(),
                project_id: project.map(str::to_owned),
                access_token: "public-synthetic-access".into(),
                refresh_token: Some("public-synthetic-refresh".into()),
                expires_at: expires,
            },
        )
        .unwrap();
    drop(lock);
    let config = Config {
        base_url: url.clone(),
        token_url: url.join("/token").unwrap(),
        client_id: Some("public-synthetic-client".into()),
        project: project.map(str::to_owned),
        ..Config::default()
    };
    (
        super::antigravity::AntigravityProvider::with_auth(
            super::antigravity::auth::Auth::new(config, store.clone()).unwrap(),
        ),
        store,
    )
}

async fn pure_count_response(
    provider: &super::antigravity::AntigravityProvider,
    body: &crate::anthropic::schema::MessagesRequest,
) -> (http::StatusCode, Value) {
    use super::antigravity::request::effects;
    use crate::provider::Provider;
    let before = provider.count_scope_snapshot();
    effects::reset();
    let response = provider
        .handle_count_tokens(body.clone(), metadata_count_context())
        .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        effects::snapshot(),
        [0; 5],
        "no credential/profile load, access, factory, clock or native identity source"
    );
    assert_eq!(
        provider.count_scope_snapshot(),
        before,
        "count cannot publish or mutate scope"
    );
    (status, value)
}

#[test]
fn antigravity_agy_count_value_inputs_preserve_semantics_without_effects() {
    use super::antigravity::request::{self, CountInputs, ServingScope, effects};
    let lineage = default_lineage();
    let contents =
        json!([{"role":"model","parts":[{"text":"prior","thoughtSignature":"untouched"}]}]);
    let full = encode_turn(&contents, &binding(), &lineage, 0, "full", &json!([])).unwrap();
    let omitted = encode_turn(&contents, &binding(), &lineage, 0, "omitted", &json!([])).unwrap();
    let known = ServingScope {
        profile_id: "profile".into(),
        project_id: "project".into(),
    };
    let wrong = ServingScope {
        profile_id: "not-the-serving-profile".into(),
        project_id: "project".into(),
    };
    let mut expected = None;
    for blocks in [full, omitted] {
        let mut value = request_value();
        value["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":blocks}),
            json!({"role":"user","content":"next"}),
        ]);
        let body: crate::anthropic::schema::MessagesRequest =
            serde_json::from_value(value).unwrap();
        let before = serde_json::to_vec(&body).unwrap();
        for scope in [None, Some(&known)] {
            effects::reset();
            let inputs = CountInputs {
                serving_scope: scope,
                configured_project: Some("project"),
            };
            let count = request::estimate(&body, inputs).unwrap();
            assert_eq!(request::estimate(&body, inputs).unwrap(), count);
            assert!(count > 0);
            if let Some(expected) = expected {
                assert_eq!(
                    count, expected,
                    "opaque/version/state bytes are not semantic token input"
                );
            }
            expected = Some(count);
            assert_eq!(
                serde_json::to_vec(&body).unwrap(),
                before,
                "count never mutates caller history/lineage"
            );
            assert_eq!(effects::snapshot(), [0; 5]);
        }
        effects::reset();
        assert!(
            request::estimate(
                &body,
                CountInputs {
                    serving_scope: Some(&wrong),
                    configured_project: Some("project")
                }
            )
            .is_err()
        );
        assert!(
            request::estimate(
                &body,
                CountInputs {
                    serving_scope: None,
                    configured_project: Some("other-project")
                }
            )
            .is_err()
        );
        let mut changed_model = body.clone();
        changed_model.model = Some("antigravity/gemini-4-argon-high".into());
        assert!(
            request::estimate(
                &changed_model,
                CountInputs {
                    serving_scope: None,
                    configured_project: Some("project")
                }
            )
            .is_ok()
        );
        changed_model.model = Some("antigravity/unsupported-model".into());
        assert!(
            request::estimate(
                &changed_model,
                CountInputs {
                    serving_scope: None,
                    configured_project: Some("project")
                }
            )
            .is_err()
        );
        assert_eq!(effects::snapshot(), [0; 5]);
    }
    effects::reset();
    let fresh = serde_json::from_value(request_value()).unwrap();
    assert!(
        request::estimate(
            &fresh,
            CountInputs {
                serving_scope: None,
                configured_project: None
            }
        )
        .unwrap()
            > 0,
        "unknown scope is estimate-only, not admission"
    );
    assert_eq!(effects::snapshot(), [0; 5]);
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_agy_count_never_loads_credentials_accesses_or_constructs_factories() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let server =
        super::antigravity_http_tests::Server::start(axum::Router::new().fallback(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                http::StatusCode::INTERNAL_SERVER_ERROR
            }
        }))
        .await;
    let directory = tempfile::tempdir().unwrap();
    let (provider, _) =
        counting_provider(directory.path(), &server.url, Some("project"), u64::MAX).await;
    let path = directory.path().join("antigravity/auth.json");
    let body = serde_json::from_value(request_value()).unwrap();
    for kind in ["invalid", "unreadable", "missing"] {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        match kind {
            "invalid" => std::fs::write(&path, b"public invalid synthetic auth").unwrap(),
            "unreadable" => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
                        .unwrap();
                }
            }
            "missing" => std::fs::remove_file(&path).unwrap(),
            _ => unreachable!(),
        }
        let (status, counted) = pure_count_response(&provider, &body).await;
        assert_eq!(status, 200, "{kind}");
        assert!(counted["input_tokens"].as_u64().unwrap() > 0);
        if kind == "missing" {
            assert!(!path.exists());
        }
        assert!(provider.count_scope_snapshot().is_none());
    }
    let (expired, _) = counting_provider(directory.path(), &server.url, Some("project"), 1).await;
    assert_eq!(pure_count_response(&expired, &body).await.0, 200);
    let (undiscovered, _) = counting_provider(directory.path(), &server.url, None, u64::MAX).await;
    assert_eq!(pure_count_response(&undiscovered, &body).await.0, 200);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no refresh/discovery/generation transport calls"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_agy_count_claims_are_estimates_never_authoritative_scope() {
    use crate::provider::Provider;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let server =
        super::antigravity_http_tests::Server::start(axum::Router::new().fallback(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                http::StatusCode::INTERNAL_SERVER_ERROR
            }
        }))
        .await;
    let directory = tempfile::tempdir().unwrap();
    let (provider, _) =
        counting_provider(directory.path(), &server.url, Some("project"), u64::MAX).await;
    let lineage = default_lineage();
    let mut claim = binding();
    claim.profile_id = "untrusted-carrier-claim".into();
    let blocks = encode_turn(
        &json!([{"role":"model","parts":[{"text":"prior"}]}]),
        &claim,
        &lineage,
        0,
        "full",
        &json!([]),
    )
    .unwrap();
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":blocks}),
        json!({"role":"user","content":"next"}),
    ]);
    let body = serde_json::from_value(body).unwrap();
    let (status, counted) = pure_count_response(&provider, &body).await;
    assert_eq!(status, 200);
    assert!(counted["input_tokens"].as_u64().unwrap() > 0);
    assert!(
        provider.count_scope_snapshot().is_none(),
        "untrusted carrier cannot bootstrap authoritative scope"
    );
    let rejected = provider
        .handle_messages(body, metadata_count_context())
        .await;
    assert_eq!(
        rejected.status(),
        400,
        "generation still validates actual native owner/profile binding"
    );
    assert!(provider.count_scope_snapshot().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_agy_count_scope_is_published_only_by_validated_serving() {
    use super::antigravity::request::ServingScope;
    use crate::provider::Provider;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let frame = format!(
        "data: {}\n\n",
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"completed"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5}}})
    );
    let server =
        super::antigravity_http_tests::Server::start(axum::Router::new().fallback(move || {
            let observed = observed.clone();
            let frame = frame.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                ([("content-type", "text/event-stream")], frame)
            }
        }))
        .await;
    let directory = tempfile::tempdir().unwrap();
    let (provider, _) =
        counting_provider(directory.path(), &server.url, Some("project"), u64::MAX).await;
    assert!(provider.count_scope_snapshot().is_none());
    let generated = provider
        .handle_messages(
            serde_json::from_value(request_value()).unwrap(),
            metadata_count_context(),
        )
        .await;
    assert_eq!(generated.status(), 200);
    let generated: Value = serde_json::from_slice(
        &axum::body::to_bytes(generated.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        provider.count_scope_snapshot(),
        Some(ServingScope {
            profile_id: "profile".into(),
            project_id: "project".into()
        })
    );
    std::fs::write(
        directory.path().join("antigravity/auth.json"),
        b"public invalid synthetic auth after successful serving",
    )
    .unwrap();
    let mut body = request_value();
    body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":generated["content"]}),
        json!({"role":"user","content":"next"}),
    ]);
    let body = serde_json::from_value(body).unwrap();
    assert_eq!(pure_count_response(&provider, &body).await.0, 200);
    let mut foreign_scope = binding();
    foreign_scope.profile_id = "untrusted-carrier-claim".into();
    let lineage = default_lineage();
    let foreign = encode_turn(
        &json!([{"role":"model","parts":[{"text":"foreign"}]}]),
        &foreign_scope,
        &lineage,
        0,
        "full",
        &json!([]),
    )
    .unwrap();
    let mut foreign_body = request_value();
    foreign_body["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":foreign}),
        json!({"role":"user","content":"next"}),
    ]);
    assert_eq!(
        pure_count_response(&provider, &serde_json::from_value(foreign_body).unwrap())
            .await
            .0,
        400,
        "a carrier cannot replace known serving scope"
    );
    let rejected = provider
        .handle_messages(body, metadata_count_context())
        .await;
    assert_eq!(
        rejected.status(),
        401,
        "count snapshot never authorizes generation"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "count and failed auth do not call upstream"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn antigravity_agy_failed_and_cancelled_turns_do_not_publish_count_scope() {
    use crate::provider::Provider;
    use futures_util::StreamExt;
    for cancel in [false, true] {
        let server=super::antigravity_http_tests::Server::start(axum::Router::new().fallback(move || async move {
            let body=if cancel {
                let stream=futures_util::stream::once(async { Ok::<_,std::convert::Infallible>(bytes::Bytes::from_static(b"data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"incomplete\"}]}}]}}\n\n")) }).chain(futures_util::stream::pending());
                axum::body::Body::from_stream(stream)
            } else { axum::body::Body::from("data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":false}}]}}\n\n") };
            axum::response::Response::builder().header("content-type","text/event-stream").body(body).unwrap()
        })).await;
        let directory = tempfile::tempdir().unwrap();
        let (provider, _) =
            counting_provider(directory.path(), &server.url, Some("project"), u64::MAX).await;
        let mut body = request_value();
        body["stream"] = json!(cancel);
        let response = provider
            .handle_messages(
                serde_json::from_value(body).unwrap(),
                metadata_count_context(),
            )
            .await;
        assert_eq!(response.status(), if cancel { 200 } else { 502 });
        drop(response);
        tokio::task::yield_now().await;
        assert!(
            provider.count_scope_snapshot().is_none(),
            "no successful native checkpoint, no authoritative count scope"
        );
    }
}

#[test]
fn antigravity_absent_native_args_project_empty_without_changing_replay() {
    let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"empty"},"thoughtSignature":"untouched"}]}]);
    let set = ToolSet::compile(
        &json!([{"name":"empty","input_schema":{"type":"object","additionalProperties":false}}]),
    )
    .unwrap();
    let mapping = set.mapping(&contents).unwrap();
    let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
    assert_eq!(blocks[1]["input"], json!({}));
    let decoded = replay::decode(&blocks, &binding()).unwrap();
    assert_eq!(decoded["contents"], contents);
    assert!(
        decoded["contents"][0]["parts"][0]["functionCall"]
            .get("args")
            .is_none()
    );
    let mut continuation = request_value();
    continuation["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":blocks}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":blocks[1]["id"],"content":"completed"}]}),
    ]);
    let prepared = prepare(&serde_json::from_value(continuation).unwrap(), &binding()).unwrap();
    assert_eq!(prepared.wire["request"]["contents"][1], contents[0]);
    assert_eq!(
        prepared.wire["request"]["contents"][2],
        json!({"role":"model","parts":[{"functionResponse":{"name":"empty","response":{"output":"completed"}}}]})
    );
    let mut reducer = Reducer::default();
    reducer.push(format!("data: {}\n\n", json!({"response":{"candidates":[{"content":contents[0],"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}})).as_bytes()).unwrap();
    assert_eq!(reducer.finish().unwrap().contents, contents);
    let required = ToolSet::compile(&json!([{"name":"empty","input_schema":{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]}}])).unwrap();
    assert!(required.mapping(&contents).is_err());
    for args in [Value::Null, json!([]), json!(""), json!(1)] {
        let mut invalid = contents.clone();
        invalid[0]["parts"][0]["functionCall"]["args"] = args;
        assert!(set.mapping(&invalid).is_err());
    }
    for key in ["partialArgs", "willContinue"] {
        let mut invalid = contents.clone();
        invalid[0]["parts"][0]["functionCall"][key] = json!(true);
        assert!(set.mapping(&invalid).is_err());
    }
}

#[test]
fn antigravity_json_fallback_preserves_complete_bounded_schema_guidance() {
    for schema in [
        json!({"type":"object","properties":{"known":{"type":"string"}},"required":["mandatory"],"additionalProperties":{"type":"integer","enum":[17,29]}}),
        json!({"type":"object","properties":{"value":{"type":"string","pattern":"^prefix"}},"oneOf":[{"required":["value"]},{"required":["other"]}]}),
    ] {
        let set = ToolSet::compile(&json!([{"name":"wrapped","input_schema":schema}])).unwrap();
        let description = set.declarations[0]["parameters"]["properties"]["json"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains(&serde_json::to_string(&schema).unwrap()),
            "original constraints missing from fallback: {description}"
        );
    }
    let source = json!({"type":"object","additionalProperties":{"type":"string"},"description":"x".repeat(20000)});
    assert!(
        ToolSet::compile(&json!([{"name":"oversized","input_schema":source}])).is_err(),
        "oversized faithful guidance must reject before inference"
    );
}

#[test]
fn antigravity_required_undeclared_properties_use_codec_or_reject_unsatisfiable_contract() {
    let schema =
        json!({"type":"object","properties":{"known":{"type":"string"}},"required":["mandatory"]});
    let set = ToolSet::compile(&json!([{"name":"caller","input_schema":schema}])).unwrap();
    assert_eq!(
        set.declarations[0]["parameters"]["required"],
        json!(["json"])
    );
    assert!(set.mapping(&json!([{"role":"model","parts":[{"functionCall":{"name":"caller","args":{"json":"{\"mandatory\":17}"}}}]}])).is_ok());
    let mut closed = schema;
    closed["additionalProperties"] = json!(false);
    assert!(ToolSet::compile(&json!([{"name":"caller","input_schema":closed}])).is_err());
    closed["oneOf"] = json!([{"required":["mandatory"]}]);
    assert!(
        ToolSet::compile(&json!([{"name":"caller","input_schema":closed}])).is_err(),
        "a codec fallback must not hide the known unsatisfiable required-property contract"
    );
}

#[test]
fn antigravity_nested_freeform_guidance_keeps_required_names_and_original_validation() {
    let leaf = json!({"type":"object","required":["weight"],"additionalProperties":{"type":"integer","enum":[17,29]}});
    let set = ToolSet::compile(&json!([{"name":"nested","input_schema":{"type":"object","properties":{"payload":leaf},"required":["payload"]}}])).unwrap();
    let native = &set.declarations[0]["parameters"]["properties"]["payload"];
    assert_eq!(native["type"], "STRING");
    assert!(
        native["description"]
            .as_str()
            .unwrap()
            .contains(&serde_json::to_string(&leaf).unwrap())
    );
    for (raw, valid) in [
        (r#"{"weight":17}"#, true),
        (r#"{"weight":18}"#, false),
        ("{}", false),
    ] {
        let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"nested","args":{"payload":raw}}}]}]);
        assert_eq!(set.mapping(&contents).is_ok(), valid);
    }
}

#[test]
fn antigravity_typeless_anyof_keeps_every_sibling_in_complete_codec_guidance() {
    let cases = [
        ("enum", json!({"enum":[17,29]}), json!(18)),
        ("const", json!({"const":17}), json!(18)),
        (
            "allOf",
            json!({"allOf":[{"minimum":17},{"maximum":17}]}),
            json!(18),
        ),
        (
            "oneOf",
            json!({"oneOf":[{"const":17},{"const":29}]}),
            json!(18),
        ),
        (
            "description",
            json!({"description":"An integer or fractional number; preserve this guidance."}),
            json!("not a number"),
        ),
        (
            "annotations",
            json!({"title":"numeric choice","default":17}),
            json!("not a number"),
        ),
        ("type array", json!({"type":["integer"]}), json!(17.5)),
    ];
    let mut missing = Vec::new();
    let mut compiled = Vec::new();
    for (name, siblings, invalid) in cases {
        let mut leaf = json!({"anyOf":[{"type":"integer"},{"type":"number"}]});
        leaf.as_object_mut()
            .unwrap()
            .extend(siblings.as_object().unwrap().clone());
        let set = ToolSet::compile(&json!([{"name":"union","input_schema":{"type":"object","properties":{"value":leaf},"required":["value"]}}])).unwrap();
        let native = &set.declarations[0]["parameters"]["properties"]["value"];
        if native["type"] != "STRING"
            || !native["description"]
                .as_str()
                .is_some_and(|text| text.contains(&serde_json::to_string(&leaf).unwrap()))
        {
            missing.push(name);
        }
        compiled.push((set, invalid));
    }
    assert!(
        missing.is_empty(),
        "typeless anyOf discarded complete sibling guidance: {missing:?}"
    );
    for (set, invalid) in compiled {
        let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"union","args":{"value":"17"}},"thoughtSignature":"unchanged"}]}]);
        let mapping = set.mapping(&contents).unwrap();
        assert_eq!(mapping[0]["codec_paths"], json!(["/value"]));
        let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
        assert_eq!(blocks[1]["input"]["value"], 17);
        assert_eq!(
            replay::decode(&blocks, &binding()).unwrap()["contents"],
            contents
        );
        let mut invalid_contents = contents;
        invalid_contents[0]["parts"][0]["functionCall"]["args"]["value"] =
            json!(serde_json::to_string(&invalid).unwrap());
        assert!(
            set.mapping(&invalid_contents).is_err(),
            "decoded arguments must still satisfy the complete original schema"
        );
    }
}

#[test]
fn antigravity_typeless_anyof_keeps_object_siblings_without_narrowing_or_dropping_them() {
    let leaf = json!({
        "anyOf":[{"type":"object","properties":{"x":{"type":"integer"}}},{"type":"object","properties":{"y":{"type":"string"}}}],
        "properties":{"x":{"type":"integer"}},"required":["x"],"additionalProperties":false
    });
    let set = ToolSet::compile(&json!([{"name":"union","input_schema":{"type":"object","properties":{"value":leaf},"required":["value"]}}])).unwrap();
    let native = &set.declarations[0]["parameters"]["properties"]["value"];
    assert_eq!(native["type"], "STRING");
    assert!(
        native["description"]
            .as_str()
            .unwrap()
            .contains(&serde_json::to_string(&leaf).unwrap())
    );
    for (raw, valid) in [
        (r#"{"x":17}"#, true),
        (r#"{"y":"other"}"#, false),
        (r#"{"x":17,"extra":true}"#, false),
    ] {
        let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"union","args":{"value":raw}}}]}]);
        assert_eq!(set.mapping(&contents).is_ok(), valid);
    }
}

#[test]
fn antigravity_typeless_anyof_sibling_guidance_bound_and_reference_rejection_still_apply() {
    let oversized =
        json!({"anyOf":[{"type":"integer"},{"type":"number"}],"description":"x".repeat(20000)});
    assert!(ToolSet::compile(&json!([{"name":"union","input_schema":{"type":"object","properties":{"value":oversized}}}])).is_err(), "complete anyOf sibling guidance cannot bypass the 16 KiB bound");
    for rejected in [
        json!({"$ref":"https://example.invalid/schema"}),
        json!({"$defs":{}}),
        json!({"$dynamicRef":"#x"}),
    ] {
        let mut leaf = json!({"anyOf":[{"type":"integer"},{"type":"number"}]});
        leaf.as_object_mut()
            .unwrap()
            .extend(rejected.as_object().unwrap().clone());
        assert!(ToolSet::compile(&json!([{"name":"union","input_schema":{"type":"object","properties":{"value":leaf}}}])).is_err());
    }
}

#[test]
fn antigravity_typeless_anyof_without_siblings_retains_native_alternatives() {
    let set = ToolSet::compile(&json!([{"name":"union","input_schema":{"type":"object","properties":{"value":{"anyOf":[{"type":"integer"},{"type":"number"}]}}}}])).unwrap();
    assert_eq!(
        set.declarations[0]["parameters"]["properties"]["value"],
        json!({"anyOf":[{"type":"INTEGER"},{"type":"NUMBER"}]})
    );
    let contents =
        json!([{"role":"model","parts":[{"functionCall":{"name":"union","args":{"value":17.5}}}]}]);
    let mapping = set.mapping(&contents).unwrap();
    assert_eq!(mapping[0]["codec_paths"], json!([]));
}

#[test]
fn antigravity_nonstring_enum_values_survive_native_guidance() {
    let set = ToolSet::compile(&json!([{"name":"numbers","input_schema":{"type":"object","properties":{"value":{"type":"integer","enum":[17,29]}}}}])).unwrap();
    let description = set.declarations[0]["parameters"]["properties"]["value"]["description"]
        .as_str()
        .unwrap();
    assert!(
        description.contains("[17,29]"),
        "native enum guidance omitted accepted values: {description}"
    );
    assert!(set.mapping(&json!([{"role":"model","parts":[{"functionCall":{"name":"numbers","args":{"value":18}}}]}])).is_err());
}

#[test]
fn antigravity_adjacent_assistant_fragments_preserve_original_system_boundaries() {
    let contents =
        json!([{"role":"model","parts":[{"thought":true,"text":"thought"},{"text":"answer"}]}]);
    let blocks = encode_default(&contents, &binding(), "full", &json!([])).unwrap();
    let mut request = request_value();
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":[blocks[0]]}),
        json!({"role":"assistant","content":"answer"}),
        json!({"role":"user","content":"next"}),
    ]);
    let prepared = prepare(
        &serde_json::from_value(request.clone()).unwrap(),
        &binding(),
    )
    .unwrap();
    assert_eq!(prepared.wire["request"]["contents"][1], contents[0]);
    let mut with_empty_fragment = request.clone();
    with_empty_fragment["messages"]
        .as_array_mut()
        .unwrap()
        .insert(1, json!({"role":"assistant","content":[]}));
    assert!(
        prepare(
            &serde_json::from_value(with_empty_fragment).unwrap(),
            &binding()
        )
        .is_ok(),
        "empty array fragments must not invent unsigned turns"
    );
    request["messages"]
        .as_array_mut()
        .unwrap()
        .insert(2, json!({"role":"system","content":"real system boundary"}));
    assert!(
        prepare(&serde_json::from_value(request).unwrap(), &binding()).is_err(),
        "system extraction must not manufacture adjacency"
    );

    let mut distinct = request_value();
    distinct["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":blocks}),
        json!({"role":"system","content":"privileged"}),
        json!({"role":"user","content":"<system-reminder>ordinary user text</system-reminder>"}),
    ]);
    let prepared = prepare(&serde_json::from_value(distinct).unwrap(), &binding()).unwrap();
    assert_eq!(
        prepared.wire["request"]["contents"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        prepared.wire["request"]["systemInstruction"]["parts"],
        json!([{"text":"privileged"}])
    );
    assert_eq!(prepared.wire["request"]["contents"][2]["role"], "user");
}

#[test]
fn antigravity_count_estimate_is_independent_of_thought_display_and_fragmentation() {
    let contents = json!([{"role":"model","parts":[{"thought":true,"text":"retained hidden thought ".repeat(200)},{"text":"answer","thoughtSignature":"opaque"}]}]);
    let mut counts = Vec::new();
    for display in ["full", "omitted"] {
        let blocks = encode_default(&contents, &binding(), display, &json!([])).unwrap();
        let mut request = request_value();
        request["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":blocks}),
            json!({"role":"user","content":"next"}),
        ]);
        counts.push(
            prepare(&serde_json::from_value(request).unwrap(), &binding())
                .unwrap()
                .estimate,
        );
    }
    assert!(
        counts.iter().all(|n| *n == counts[0]),
        "presentation changed estimated native history: {counts:?}"
    );
    for display in ["full", "omitted"] {
        let blocks = encode_default(&contents, &binding(), display, &json!([])).unwrap();
        let mut request = request_value();
        request["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":[blocks[0]]}),
            json!({"role":"assistant","content":"answer"}),
            json!({"role":"user","content":"next"}),
        ]);
        assert_eq!(
            prepare(&serde_json::from_value(request).unwrap(), &binding())
                .unwrap()
                .estimate,
            counts[0]
        );
    }
}

#[test]
fn antigravity_count_reconstructs_recorded_name_and_json_codecs_without_transport_duplication() {
    let vectors = golden();
    let mut estimates = Vec::new();
    for index in [0, 2] {
        let vector = &vectors["vectors"][index];
        let scope: replay::Binding = serde_json::from_value(vector["binding"].clone()).unwrap();
        for display in ["full", "omitted"] {
            for signature_bytes in [1, 100000] {
                let mut native = vector["native_contents"].clone();
                native[0]["parts"][0]["thoughtSignature"] = json!("x".repeat(signature_bytes));
                let blocks = encode_default(
                    &native,
                    &scope,
                    display,
                    &vector["envelope"]["tool_mapping"],
                )
                .unwrap();
                for split in [false, true] {
                    let mut request = request_value();
                    if split {
                        request["messages"].as_array_mut().unwrap().extend([
                            json!({"role":"assistant","content":[blocks[0]]}),
                            json!({"role":"assistant","content":[blocks[1]]}),
                        ]);
                    } else {
                        request["messages"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"role":"assistant","content":blocks}));
                    }
                    request["messages"].as_array_mut().unwrap().push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":blocks[1]["id"],"content":"ok"}]}));
                    estimates.push(
                        prepare(&serde_json::from_value(request).unwrap(), &scope)
                            .unwrap()
                            .estimate,
                    );
                }
            }
        }
    }
    assert!(
        estimates.iter().all(|estimate| *estimate == estimates[0]),
        "codec/display/fragment representation changed semantic estimate: {estimates:?}"
    );
}

#[test]
fn antigravity_large_text_coalescing_preserves_unicode_and_unknown_field_checks() {
    let text = "雪a".repeat(256);
    let contents = json!([{"role":"model","parts":vec![json!({"text":text});4096]}]);
    let mapping = json!([]);
    let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
    let envelope = replay::decode(&blocks, &binding()).unwrap();
    let merged = json!([blocks[0], {"type":"text","text":text.repeat(4096)}]);
    let start = std::time::Instant::now();
    let fragmented_digest = replay::digest_input(&blocks, &envelope).unwrap();
    eprintln!(
        "antigravity_coalescing_4096_parts_us={}",
        start.elapsed().as_micros()
    );
    assert_eq!(
        fragmented_digest,
        replay::digest_input(&merged, &envelope).unwrap()
    );
    let invalid = json!([blocks[0], {"type":"text","text":"a"},{"type":"text","text":"b","unsupported":true}]);
    assert!(replay::digest_input(&invalid, &envelope).is_err());
}

#[test]
fn antigravity_coalescing_rejects_cumulative_oversize_before_growth() {
    let vector = golden();
    let envelope = &vector["vectors"][0]["envelope"];
    let blocks = json!([{"type":"thinking","thinking":""},{"type":"text","text":"x".repeat(replay::MAX_DECODED)},{"type":"text","text":"b"}]);
    assert!(replay::digest_input(&blocks, envelope).is_err());
}

#[test]
fn antigravity_integer_exclusive_projection_keeps_fractional_inclusive_constraints() {
    let set = ToolSet::compile(
        &json!([{"name":"range","input_schema":{"type":"object","properties":{
            "lower":{"type":"integer","minimum":1.5,"exclusiveMinimum":0},
            "upper":{"type":"integer","maximum":-1.5,"exclusiveMaximum":0}
        }}}]),
    )
    .unwrap();
    let properties = &set.declarations[0]["parameters"]["properties"];
    assert_eq!(
        properties,
        &json!({
            "lower":{"type":"INTEGER","minimum":1.5},
            "upper":{"type":"INTEGER","maximum":-1.5}
        })
    );
    let call = |args: Value| json!([{"role":"model","parts":[{"functionCall":{"name":"range","args":args}}]}]);
    assert!(set.mapping(&call(json!({"lower":2,"upper":-2}))).is_ok());
    for args in [
        json!({"lower":1,"upper":-2}),
        json!({"lower":1.5,"upper":-2}),
        json!({"lower":2,"upper":-1}),
        json!({"lower":2,"upper":-1.5}),
    ] {
        assert!(set.mapping(&call(args)).is_err());
    }
}

#[test]
fn antigravity_selected_scalar_constraints_keep_proto_types_and_original_validation() {
    let set = ToolSet::compile(&json!([{"name":"bounded","input_schema":{
        "type":"object","properties":{
            "text":{"type":"string","minLength":1,"maxLength":3},
            "list":{"type":"array","items":{"type":"integer"},"minItems":1,"maxItems":2},
            "number":{"type":"number","minimum":1,"maximum":4.0}
        },"required":["text","list","number"],"additionalProperties":false
    }}]))
    .unwrap();
    let parameters = &set.declarations[0]["parameters"];
    assert_eq!(parameters.as_object().unwrap().len(), 3);
    assert_eq!(parameters["type"], "OBJECT");
    assert_eq!(parameters["required"], json!(["text", "list", "number"]));
    let properties = &parameters["properties"];
    assert_eq!(properties.as_object().unwrap().len(), 3);
    assert_eq!(
        properties["text"],
        json!({"type":"STRING","minLength":"1","maxLength":"3"})
    );
    assert_eq!(
        properties["list"],
        json!({"type":"ARRAY","items":{"type":"INTEGER"},"minItems":"1","maxItems":"2"})
    );
    let number = properties["number"].as_object().unwrap();
    assert_eq!(number.len(), 3);
    assert_eq!(number["type"], "NUMBER");
    assert_eq!(number["minimum"].as_f64(), Some(1.0));
    assert_eq!(number["maximum"].as_f64(), Some(4.0));
    let call = |args: Value| json!([{"role":"model","parts":[{"functionCall":{"name":"bounded","args":args}}]}]);
    assert!(
        set.mapping(&call(json!({"text":"ok","list":[1,2],"number":2.5})))
            .is_ok()
    );
    for args in [
        json!({"text":"","list":[1],"number":2}),
        json!({"text":"long","list":[1],"number":2}),
        json!({"text":"ok","list":[],"number":2}),
        json!({"text":"ok","list":[1,2,3],"number":2}),
        json!({"text":"ok","list":[1.5],"number":2}),
        json!({"text":"ok","list":[1],"number":0.5}),
        json!({"text":"ok","list":[1],"number":4.5}),
        json!({"text":"ok","list":[1],"number":2,"extra":true}),
        json!({"text":"ok","list":[1]}),
    ] {
        assert!(set.mapping(&call(args)).is_err());
    }
}

fn property_names_tool(schema: Value) -> ToolSet {
    ToolSet::compile(&json!([{"name":"record","input_schema":schema}])).unwrap()
}

fn property_names_call(args: Value) -> Value {
    json!([{"role":"model","parts":[{"functionCall":{"name":"record","args":args},"thoughtSignature":"unchanged"}]}])
}

#[test]
fn antigravity_property_names_records_keep_complete_codec_and_replay() {
    let record =
        json!({"type":"object","propertyNames":{"type":"string"},"additionalProperties":{}});
    let set = property_names_tool(
        json!({"type":"object","properties":{"metadata":record},"required":["metadata"]}),
    );
    let native = &set.declarations[0]["parameters"]["properties"]["metadata"];
    assert_eq!(native["type"], "STRING");
    assert!(
        native["description"]
            .as_str()
            .unwrap()
            .contains(&serde_json::to_string(&record).unwrap())
    );
    let metadata = json!({"a/b~雪":{"nested":[null,true,17,"value"]},"empty":{},"":[]});
    let contents =
        property_names_call(json!({"metadata":serde_json::to_string(&metadata).unwrap()}));
    let mapping = set.mapping(&contents).unwrap();
    assert_eq!(mapping[0]["codec_paths"], json!(["/metadata"]));
    let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
    assert_eq!(blocks[1]["input"], json!({"metadata":metadata}));
    assert_eq!(
        replay::decode(&blocks, &binding()).unwrap()["contents"],
        contents
    );
    for raw in ["{", r#"{"x":1,"x":2}"#, "null", "[]"] {
        assert!(
            set.mapping(&property_names_call(json!({"metadata":raw})))
                .is_err()
        );
    }
}

#[test]
fn antigravity_property_names_static_cli_record_shapes_compile_without_weakening() {
    // Relevant record shapes from the pinned CLI's static tool definitions, not
    // a claim that every optional tool is present in every actual-CLI request.
    let metadata =
        json!({"type":"object","propertyNames":{"type":"string"},"additionalProperties":{}});
    let answers = json!({"type":"object","propertyNames":{"type":"string"},"additionalProperties":{"type":"string"}});
    let annotations = json!({"type":"object","propertyNames":{"type":"string"},"additionalProperties":{
        "type":"object","properties":{"preview":{"type":"string"},"notes":{"type":"string"}},"additionalProperties":false
    }});
    let set = ToolSet::compile(&json!([
        {"name":"TaskCreate","input_schema":{"type":"object","properties":{"metadata":metadata},"additionalProperties":false}},
        {"name":"TaskUpdate","input_schema":{"type":"object","properties":{"metadata":metadata},"additionalProperties":false}},
        {"name":"AskUserQuestion","input_schema":{"type":"object","properties":{"answers":answers,"annotations":annotations},"additionalProperties":false}}
    ])).unwrap();
    for (index, property, source) in [
        (0, "metadata", &metadata),
        (1, "metadata", &metadata),
        (2, "answers", &answers),
        (2, "annotations", &annotations),
    ] {
        let native = &set.declarations[index]["parameters"]["properties"][property];
        assert_eq!(native["type"], "STRING");
        assert!(
            native["description"]
                .as_str()
                .unwrap()
                .contains(&serde_json::to_string(source).unwrap())
        );
    }
    for (answers, annotations, valid) in [
        (
            json!({"question":"answer"}),
            json!({"question":{"preview":"preview","notes":"notes"}}),
            true,
        ),
        (json!({}), json!({"question":{}}), true),
        (json!({"question":17}), json!({}), false),
        (json!({}), json!({"question":{"preview":17}}), false),
        (
            json!({}),
            json!({"question":{"extra":"not allowed"}}),
            false,
        ),
    ] {
        let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"AskUserQuestion","args":{
            "answers":serde_json::to_string(&answers).unwrap(),"annotations":serde_json::to_string(&annotations).unwrap()
        }}}]}]);
        assert_eq!(set.mapping(&contents).is_ok(), valid);
    }
}

#[test]
fn antigravity_property_names_structured_guidance_validates_every_key_and_value() {
    for dialect in [
        "https://json-schema.org/draft/2020-12/schema",
        "http://json-schema.org/draft-07/schema#",
    ] {
        let names = json!({"type":"string","pattern":"^ok","minLength":3});
        let schema = json!({"$schema":dialect,"type":"object","properties":{
            "ok_known":{"type":"integer"},"bad_known":{"type":"integer"},
            "ok_default":{"type":"integer","default":17}
        },"propertyNames":names});
        let set = property_names_tool(schema);
        let parameters = &set.declarations[0]["parameters"];
        assert_eq!(parameters["type"], "OBJECT");
        assert!(
            parameters["description"]
                .as_str()
                .unwrap()
                .contains(&format!(
                    "\"propertyNames\":{}",
                    serde_json::to_string(&names).unwrap()
                ))
        );
        for args in [json!({}), json!({"ok_known":17}), json!({"ok_extra":true})] {
            let contents = property_names_call(args.clone());
            let mapping = set.mapping(&contents).unwrap();
            assert_eq!(mapping[0]["codec_paths"], json!([]));
            let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
            assert_eq!(blocks[1]["input"], args, "defaults must not be inserted");
        }
        for args in [
            json!({"bad_known":17}),
            json!({"bad_extra":17}),
            json!({"ok":17}),
            json!({"ok_known":"17"}),
        ] {
            assert_eq!(
                set.mapping(&property_names_call(args)).err(),
                Some("upstream tool arguments violate original schema")
            );
        }
    }
}

#[test]
fn antigravity_property_names_standard_applicability_and_boolean_schemas() {
    for (names, populated) in [
        (json!(true), true),
        (json!(false), false),
        (json!({"type":"integer"}), false),
    ] {
        let set = property_names_tool(
            json!({"type":"object","properties":{"value":{"propertyNames":names}},"required":["value"]}),
        );
        for (value, valid) in [
            (json!({}), true),
            (json!({"x":1}), populated),
            (json!(17), true),
            (json!([1]), true),
            (json!(null), true),
        ] {
            // Tool inputs remain objects; only this unconstrained nested value
            // varies, so non-object applicability is tested without violating
            // the existing root-codec object transport contract.
            let contents =
                property_names_call(json!({"value":serde_json::to_string(&value).unwrap()}));
            assert_eq!(
                set.mapping(&contents).is_ok(),
                valid,
                "legal propertyNames applicability changed for {value}"
            );
        }
    }
    let set = property_names_tool(
        json!({"type":"object","properties":{"value":{"type":"integer","propertyNames":false}},"required":["value"]}),
    );
    assert!(
        set.mapping(&property_names_call(json!({"value":17})))
            .is_ok()
    );
    assert!(
        set.mapping(&property_names_call(json!({"value":"17"})))
            .is_err()
    );
    let set = property_names_tool(
        json!({"type":"object","additionalProperties":false,"propertyNames":false}),
    );
    assert!(set.declarations[0].get("parameters").is_none());
    assert!(set.mapping(&property_names_call(json!({}))).is_ok());
    assert!(set.mapping(&property_names_call(json!({"x":1}))).is_err());
}

#[test]
fn antigravity_property_names_enums_and_freeform_value_constraints_are_original() {
    let schema = json!({"type":"object","propertyNames":{"enum":["allowed"]},"additionalProperties":{"type":"integer","minimum":1,"default":17}});
    let set = property_names_tool(schema.clone());
    let native = &set.declarations[0]["parameters"]["properties"]["json"];
    assert_eq!(native["type"], "STRING");
    assert!(
        native["description"]
            .as_str()
            .unwrap()
            .contains(&serde_json::to_string(&schema).unwrap())
    );
    for (value, valid) in [
        (json!({}), true),
        (json!({"allowed":1}), true),
        (json!({"other":1}), false),
        (json!({"allowed":0}), false),
        (json!({"allowed":"1"}), false),
        (json!({"allowed":null}), false),
    ] {
        let contents = property_names_call(json!({"json":serde_json::to_string(&value).unwrap()}));
        assert_eq!(set.mapping(&contents).is_ok(), valid);
        if valid {
            let mapping = set.mapping(&contents).unwrap();
            let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
            assert_eq!(blocks[1]["input"], value);
        }
    }
}

#[test]
fn antigravity_property_names_traversal_rejects_unsupported_descendants_and_malformed_values() {
    for child in [
        json!({"$ref":"#/$defs/name"}),
        json!({"$ref":"https://example.invalid/schema"}),
        json!({"$defs":{}}),
        json!({"definitions":{}}),
        json!({"$dynamicRef":"#name"}),
        json!({"examples":[]}),
        json!({"unknownConstraint":true}),
        json!({"anyOf":[{"type":"string"},{"$ref":"#"}]}),
    ] {
        assert_eq!(
            ToolSet::compile(&json!([{"name":"record","input_schema":{"propertyNames":child}}]))
                .err(),
            Some("unsupported schema keyword or reference")
        );
    }
    for child in [json!(null), json!(17), json!("string"), json!([])] {
        assert_eq!(
            ToolSet::compile(&json!([{"name":"record","input_schema":{"propertyNames":child}}]))
                .err(),
            Some("schema must be object or boolean")
        );
    }
    for child in [json!({"type":"invented"}), json!({"minLength":-1})] {
        assert_eq!(
            ToolSet::compile(&json!([{"name":"record","input_schema":{"propertyNames":child}}]))
                .err(),
            Some("invalid original tool schema")
        );
    }
    let set = property_names_tool(
        json!({"type":"object","properties":{"propertyNames":{"type":"string"}},"required":["propertyNames"],"additionalProperties":false}),
    );
    assert!(
        set.mapping(&property_names_call(
            json!({"propertyNames":"ordinary domain field"})
        ))
        .is_ok()
    );
}

#[test]
fn antigravity_property_names_traversal_preserves_depth_node_byte_and_guidance_limits() {
    let compile = |schema| ToolSet::compile(&json!([{"name":"record","input_schema":schema}]));
    let mut deep = json!(true);
    for _ in 0..32 {
        deep = json!({"propertyNames":deep});
    }
    assert!(compile(deep.clone()).is_ok());
    assert_eq!(
        compile(json!({"propertyNames":deep})).err(),
        Some("schema depth/node limit")
    );
    let properties = (0..8190)
        .map(|i| (format!("p{i}"), json!(true)))
        .collect::<serde_json::Map<_, _>>();
    let mut wide = json!({"type":"object","properties":properties,"propertyNames":true});
    assert!(compile(wide.clone()).is_ok());
    wide["properties"]["extra"] = json!(true);
    assert_eq!(compile(wide).err(), Some("schema depth/node limit"));
    assert_eq!(
        compile(json!({"propertyNames":{"description":"x".repeat(256*1024)}})).err(),
        Some("schema byte limit")
    );
    for schema in [
        json!({"propertyNames":{"description":"x".repeat(16384)}}),
        json!({"type":"object","properties":{"x":{"type":"integer"}},"propertyNames":{"description":"x".repeat(16384)}}),
    ] {
        assert_eq!(
            compile(schema).err(),
            Some("complete schema guidance exceeds the 16 KiB limit")
        );
    }
}

#[test]
fn antigravity_schema_preserves_original_dialects_and_constraints() {
    for dialect in [
        "https://json-schema.org/draft/2020-12/schema",
        "http://json-schema.org/draft-07/schema#",
    ] {
        let set = ToolSet::compile(&json!([{"name":"Read", "input_schema":{
            "$schema":dialect, "type":"object", "properties":{"limit":{"type":"integer","exclusiveMinimum":0}},
            "required":["limit"], "additionalProperties":false
        }}])).unwrap();
        assert_eq!(
            set.declarations[0]["parameters"],
            json!({"type":"OBJECT","properties":{"limit":{"type":"INTEGER"}},"required":["limit"]})
        );
        for args in [
            json!({"limit":0}),
            json!({"limit":-1}),
            json!({"limit":1.5}),
            json!({"limit":1,"extra":true}),
        ] {
            assert!(set.mapping(&json!([{"role":"model","parts":[{"functionCall":{"name":"Read","args":args}}]}])).is_err());
        }
        assert!(set.mapping(&json!([{"role":"model","parts":[{"functionCall":{"name":"Read","args":{"limit":1}}}]}])).is_ok());
    }
}

#[test]
fn antigravity_schema_rejects_external_references_and_unknown_keywords() {
    for schema in [
        json!({"$ref":"https://example.invalid/schema"}),
        json!({"$defs":{}}),
        json!({"definitions":{}}),
        json!({"$dynamicRef":"#x"}),
        json!({"prefixItems":[]}),
        json!({"unevaluatedProperties":false}),
        json!({"$schema":"https://example.invalid/draft"}),
        json!({"type":"object", "unknownConstraint":true}),
    ] {
        assert!(ToolSet::compile(&json!([{"name":"tool", "input_schema":schema}])).is_err());
    }
}

#[test]
fn antigravity_freeform_codec_and_mapped_names_are_lossless() {
    let name = "caller/tool with spaces";
    let set = ToolSet::compile(&json!([{"name":name,"input_schema":{"type":"object","properties":{"args":{}},"required":["args"]}}])).unwrap();
    let native_name = &set.declarations[0]["name"];
    assert_ne!(native_name, name);
    let contents = json!([{"role":"model","parts":[{"functionCall":{"name":native_name,"args":{"args":"[1, {\"x\":true}]"}},"thoughtSignature":"unchanged"}]}]);
    let mapping = set.mapping(&contents).unwrap();
    let blocks = encode_default(&contents, &binding(), "omitted", &mapping).unwrap();
    assert_eq!(blocks[1]["name"], name);
    assert_eq!(blocks[1]["input"]["args"], json!([1,{"x":true}]));
    assert_eq!(
        replay::decode(&blocks, &binding()).unwrap()["contents"],
        contents
    );
    let invalid = json!([{"role":"model","parts":[{"functionCall":{"name":native_name,"args":{"args":"{"}}}]}]);
    assert!(set.mapping(&invalid).is_err());
}

#[test]
fn antigravity_original_allof_and_format_are_validated() {
    let set = ToolSet::compile(
        &json!([{"name":"tool","input_schema":{"type":"object","properties":{
        "to":{"type":"string","allOf":[{"pattern":"^a"},{"pattern":"z$"}]},
        "address":{"type":"string","format":"email"}},"required":["to","address"]}}]),
    )
    .unwrap();
    for args in [
        json!({"to":"apple","address":"a@b.com"}),
        json!({"to":"az","address":"not email"}),
    ] {
        assert!(
            set.mapping(
                &json!([{"role":"model","parts":[{"functionCall":{"name":"tool","args":args}}]}])
            )
            .is_err()
        );
    }
}

#[test]
fn antigravity_request_accepts_observed_options_without_changing_tier() {
    let mut request = request_value();
    request["thinking"] = json!({"type":"adaptive","display":"omitted"});
    request["output_config"] = json!({"effort":"high"});
    request["context_management"] =
        json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
    let prepared = prepare(&serde_json::from_value(request).unwrap(), &binding()).unwrap();
    assert_eq!(prepared.wire["model"], "gemini-4-argon-low");
    assert_eq!(
        prepared.wire["request"]["generationConfig"]["thinkingConfig"],
        json!({"includeThoughts":true,"thinkingLevel":"LOW"})
    );
    assert_eq!(
        prepared.wire["request"]["generationConfig"]["maxOutputTokens"],
        100
    );
    assert_eq!(prepared.display, "omitted");
    assert_eq!(prepared.wire["userAgent"], "antigravity");
    assert_eq!(prepared.wire["requestType"], "agent");
    assert!(prepared.wire["request"].get("toolConfig").is_none());
    assert_eq!(prepared.wire["request"]["sessionId"], "");
}

#[test]
fn antigravity_request_system_role_user_preserves_parts_and_order() {
    let mut request = request_value();
    request["system"] = json!([{"type":"text","text":"top"}]);
    request["messages"] = json!([
        {"role":"system","content":"first"}, {"role":"user","content":"hello"},
        {"role":"system","content":[{"type":"text","text":"second","cache_control":{"type":"ephemeral"}}]}
    ]);
    let prepared = prepare(
        &serde_json::from_value(request.clone()).unwrap(),
        &binding(),
    )
    .unwrap();
    assert_eq!(
        prepared.wire["request"]["systemInstruction"],
        json!({"role":"user","parts":[{"text":"top\n\nfirst\n\nsecond"}]})
    );
    assert_eq!(
        prepared.wire["request"]["contents"],
        json!([{"role":"user","parts":[{"text":"hello"}]}])
    );
    request["messages"] = json!([{"role":"system","content":"only system"}]);
    assert!(prepare(&serde_json::from_value(request).unwrap(), &binding()).is_err());
}

#[test]
fn antigravity_request_absent_cap_defaults_to_65536() {
    for model in super::antigravity::request::MODELS {
        let mut scope = binding();
        scope.native_model = model.strip_prefix("antigravity/").unwrap().into();
        for null in [false, true] {
            let mut request = request_value();
            request["model"] = json!(model);
            request.as_object_mut().unwrap().remove("max_tokens");
            if null {
                request["max_tokens"] = Value::Null;
            }
            let prepared = prepare(&serde_json::from_value(request).unwrap(), &scope).unwrap();
            assert_eq!(
                prepared.wire["request"]["generationConfig"]["maxOutputTokens"], 65536,
                "model={model}, null={null}"
            );
        }
    }
}

#[test]
fn antigravity_request_explicit_caps_are_exact_and_invalid_caps_reject() {
    let mut estimates = Vec::new();
    for stream in [false, true] {
        for cap in [1, 512, 65536] {
            let mut request = request_value();
            request["max_tokens"] = json!(cap);
            request["stream"] = json!(stream);
            request["output_config"] = json!({"effort":"high"});
            let prepared = prepare(&serde_json::from_value(request).unwrap(), &binding()).unwrap();
            assert_eq!(
                prepared.wire["request"]["generationConfig"]["maxOutputTokens"],
                cap
            );
            assert_eq!(
                prepared.wire["request"]["generationConfig"]["thinkingConfig"],
                json!({"includeThoughts":true,"thinkingLevel":"LOW"})
            );
            estimates.push(prepared.estimate);
        }
    }
    assert!(estimates.iter().all(|estimate| *estimate == estimates[0]));
    for cap in [0, 65537] {
        let mut request = request_value();
        request["max_tokens"] = json!(cap);
        assert_eq!(
            prepare(&serde_json::from_value(request).unwrap(), &binding()).err(),
            Some("max_tokens must be between 1 and 65536")
        );
    }
    for cap in [
        json!(-1),
        json!(1.5),
        json!("512"),
        json!(true),
        json!([]),
        json!({}),
    ] {
        let mut request = request_value();
        request["max_tokens"] = cap;
        assert!(
            serde_json::from_value::<crate::anthropic::schema::MessagesRequest>(request).is_err()
        );
    }
}

#[test]
fn antigravity_request_absent_and_empty_system_keep_exact_envelope() {
    for system in [None, Some(json!([]))] {
        let mut request = request_value();
        if let Some(system) = system {
            request["system"] = system;
        }
        let prepared = prepare(&serde_json::from_value(request).unwrap(), &binding()).unwrap();
        assert_eq!(
            prepared
                .wire
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "project",
                "model",
                "request",
                "requestId",
                "userAgent",
                "requestType"
            ])
        );
        assert_eq!(
            prepared.wire["request"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["contents", "sessionId", "generationConfig"])
        );
        assert_eq!(
            prepared.wire["request"]["contents"],
            json!([{"role":"user","parts":[{"text":"hello"}]}])
        );
        assert_eq!(prepared.wire["requestType"], "agent");
        assert_eq!(prepared.wire["userAgent"], "antigravity");
    }
}

#[test]
fn antigravity_request_multiple_tools_have_one_declaration_per_object() {
    let source = json!([
        {"name":"caller/tool with spaces","description":"free-form input","input_schema":{"type":"object"}},
        {"name":"empty","input_schema":{"type":"object","additionalProperties":false}},
        {"name":"limited","input_schema":{"type":"object","properties":{"limit":{"type":"integer","minimum":1}},"required":["limit"],"additionalProperties":false}}
    ]);
    let compiled = ToolSet::compile(&source).unwrap();
    let mut request = request_value();
    request["tools"] = source;
    request["tool_choice"] = json!({"type":"auto"});
    let prepared = prepare(&serde_json::from_value(request).unwrap(), &binding()).unwrap();
    assert_eq!(prepared.tools.declarations, compiled.declarations);
    let expected: Vec<Value> = compiled
        .declarations
        .as_array()
        .unwrap()
        .iter()
        .map(|declaration| json!({"functionDeclarations":[declaration]}))
        .collect();
    assert_eq!(prepared.wire["request"]["tools"], json!(expected));
    assert_eq!(prepared.wire["request"].as_object().unwrap().len(), 5);
    assert_eq!(prepared.wire.as_object().unwrap().len(), 6);
    assert_eq!(
        prepared.wire["request"]["toolConfig"],
        json!({"functionCallingConfig":{"mode":"VALIDATED"}})
    );
    assert!(prepared.wire.get("labels").is_none());
    let contents = json!([{"role":"model","parts":[
        {"functionCall":{"name":compiled.declarations[0]["name"],"args":{"json":"{\"payload\":[1,true]}"}}},
        {"functionCall":{"name":"empty","args":{}}},
        {"functionCall":{"name":"limited","args":{"limit":1}}}
    ]}]);
    let mapping = prepared.tools.mapping(&contents).unwrap();
    let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
    assert_eq!(blocks[1]["name"], "caller/tool with spaces");
    assert_eq!(blocks[1]["input"], json!({"payload":[1,true]}));
    assert_eq!(blocks[2]["input"], json!({}));
    assert_eq!(blocks[3]["input"], json!({"limit":1}));
    assert_eq!(
        replay::decode(&blocks, &binding()).unwrap()["contents"],
        contents
    );
    for args in [
        json!({"limit":0}),
        json!({"limit":1.5}),
        json!({"limit":1,"extra":true}),
    ] {
        assert!(
            prepared
                .tools
                .mapping(&json!([{"role":"model","parts":[{"functionCall":{"name":"limited","args":args}}]}]))
                .is_err()
        );
    }
}

#[test]
fn antigravity_request_zero_one_tools_and_choice_none_preserve_omission() {
    let tool =
        json!({"name":"empty","input_schema":{"type":"object","additionalProperties":false}});
    for (source, choice, expected) in [
        (json!([]), None, None),
        (json!([]), Some(json!({"type":"auto"})), None),
        (
            json!([tool]),
            None,
            Some(
                json!([{"functionDeclarations":[{"name":"empty","parameters":{"type":"OBJECT"}}]}]),
            ),
        ),
        (json!([tool]), Some(json!({"type":"none"})), None),
    ] {
        let mut request = request_value();
        request["tools"] = source;
        if let Some(choice) = choice {
            request["tool_choice"] = choice;
        }
        let prepared = prepare(&serde_json::from_value(request).unwrap(), &binding()).unwrap();
        assert_eq!(prepared.wire["request"].get("tools"), expected.as_ref());
        assert_eq!(
            prepared.tools.declarations.as_array().unwrap().len(),
            usize::from(expected.is_some())
        );
        assert!(prepared.wire["request"].get("toolConfig").is_none());
        assert!(prepared.wire["request"].get("labels").is_none());
        assert_eq!(prepared.wire.as_object().unwrap().len(), 6);
    }
}

#[test]
fn antigravity_request_rejects_semantic_options_media_and_lost_history() {
    for (key, value) in [
        ("thinking", json!({"type":"disabled"})),
        ("thinking", json!({"type":"enabled","budget_tokens":100})),
        ("output_config", json!({"format":{"type":"json_schema"}})),
        ("output_config", json!({"effort":"invented"})),
        ("temperature", json!(0.5)),
        ("tool_choice", json!({"type":"any"})),
        (
            "context_management",
            json!({"edits":[{"type":"clear_thinking_20251015","keep":1}]}),
        ),
    ] {
        let mut request = request_value();
        request[key] = value;
        assert!(
            prepare(&serde_json::from_value(request).unwrap(), &binding()).is_err(),
            "{key}"
        );
    }
    for message in [
        json!({"role":"assistant","content":"lost signed history"}),
        json!({"role":"user","content":[{"type":"image","source":{}}]}),
        json!({"role":"system","content":[{"type":"image"}]}),
        json!({"role":"developer","content":"x"}),
    ] {
        let mut request = request_value();
        request["messages"].as_array_mut().unwrap().push(message);
        assert!(prepare(&serde_json::from_value(request).unwrap(), &binding()).is_err());
    }
}

#[test]
fn antigravity_native_sse_keeps_late_signatures_and_cumulative_usage() {
    let frames = [
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"雪"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}}}),
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"late"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":3,"candidatesTokenCount":2,"thoughtsTokenCount":4,"totalTokenCount":16}}}),
    ];
    let wire = frames
        .iter()
        .map(|v| format!("data: {v}\r\n\r\n"))
        .collect::<String>();
    let mut reducer = Reducer::default();
    for byte in wire.as_bytes() {
        reducer.push(&[*byte]).unwrap();
    }
    let turn = reducer.finish().unwrap();
    assert_eq!(turn.contents[1]["parts"][0]["thoughtSignature"], "late");
    assert_eq!(
        turn.usage,
        json!({"input_tokens":7,"cache_read_input_tokens":3,"output_tokens":6})
    );
    assert_eq!(turn.stop_reason, "end_turn");
}

#[test]
fn antigravity_native_sse_errors_never_become_successful_turns() {
    for raw in [
        "data: {\n\n",
        "data: {\"error\":{\"message\":\"private\"}}\n\n",
        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"truncated\"}]}}]}}\n\n",
    ] {
        let mut reducer = Reducer::default();
        assert!(
            reducer
                .push(raw.as_bytes())
                .and_then(|()| reducer.finish())
                .is_err()
        );
    }
}

#[test]
fn antigravity_boundaries_and_canonical_sessions_fail_closed() {
    assert!(replay::calls(&json!(vec![json!({"role":"model","parts":[]}); 513])).is_err());
    assert!(
        replay::calls(&json!([{"role":"model","parts":vec![json!({"text":""});4097]}])).is_err()
    );
    let raw = json!({"x":"0"});
    assert!(replay::decode_arguments(&raw, &json!(vec!["/x"; 65]), &Value::Null).is_err());
    assert!(
        replay::decode_arguments(
            &raw,
            &json!([format!("/{}", "x".repeat(1024))]),
            &Value::Null
        )
        .is_err()
    );
    assert!(
        replay::decode_arguments(
            &json!({"json":"{\"x\":1,\"x\":2}"}),
            &json!([]),
            &json!("json")
        )
        .is_err()
    );
    let carrier = json!([{"type":"thinking","thinking":"","signature":format!("{}{}",replay::PREFIX,"x".repeat(replay::MAX_ENCODED+1))}]);
    assert!(replay::decode(&carrier, &binding()).is_err());
    let mut reducer = Reducer::default();
    assert!(reducer.push(&vec![b'x'; 8 * 1024 * 1024 + 1]).is_err());
}

#[test]
fn antigravity_complete_result_prefix_uses_native_order_without_synthetic_continue() {
    let contents = json!([{"role":"model","parts":[
        {"functionCall":{"name":"Read","id":"native-one","args":{}},"thoughtSignature":"signed"},
        {"functionCall":{"name":"Other","args":{}}}
    ]}]);
    let mapping = replay::mapping(&contents, None).unwrap();
    let blocks = encode_default(&contents, &binding(), "full", &mapping).unwrap();
    let first =
        json!({"type":"tool_result","tool_use_id":mapping[0]["local_id"],"content":"first"});
    let second = json!({"type":"tool_result","tool_use_id":mapping[1]["local_id"],"content":[{"type":"text","text":"second"}],"is_error":true});
    let mut request = request_value();
    request["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":blocks}),
        json!({"role":"system","content":"privileged"}),
        json!({"role":"user","content":[second,first]}),
    ]);
    let prepared = prepare(
        &serde_json::from_value(request.clone()).unwrap(),
        &binding(),
    )
    .unwrap();
    assert_eq!(
        prepared.wire["request"]["contents"][2],
        json!({"role":"model","parts":[
            {"functionResponse":{"name":"Read","id":"native-one","response":{"output":"first"}}},
            {"functionResponse":{"name":"Other","response":{"error":["second"],"is_error":true}}}
        ]})
    );
    assert_eq!(
        prepared.wire["request"]["contents"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(!prepared.wire.to_string().contains("[Continue]"));
    for results in [
        json!([first]),
        json!([first, first]),
        json!([{"type":"text","text":"before"},first,second]),
        json!([first,{"type":"text","text":"between"},second]),
    ] {
        request["messages"][3]["content"] = results;
        assert!(
            prepare(
                &serde_json::from_value(request.clone()).unwrap(),
                &binding()
            )
            .is_err()
        );
    }
}

#[test]
fn antigravity_schema_root_freeform_and_closed_no_argument_tools_are_explicit() {
    let set = ToolSet::compile(&json!([
        {"name":"free","input_schema":{"type":"object"}},
        {"name":"empty","input_schema":{"type":"object","additionalProperties":false}}
    ]))
    .unwrap();
    assert_eq!(
        set.declarations[0]["parameters"]["properties"]["json"]["type"],
        "STRING"
    );
    assert_eq!(set.declarations[1]["parameters"], json!({"type":"OBJECT"}));
    let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"free","args":{"json":"{\"x\":[1,true]}"}}},{"functionCall":{"name":"empty","args":{}}}]}]);
    let map = set.mapping(&contents).unwrap();
    let blocks = encode_default(&contents, &binding(), "full", &map).unwrap();
    assert_eq!(blocks[1]["input"], json!({"x":[1,true]}));
    assert_eq!(blocks[2]["input"], json!({}));
    let invalid = json!([{"role":"model","parts":[{"functionCall":{"name":"free","args":{"json":"{}","extra":true}}}]}]);
    assert!(set.mapping(&invalid).is_err());
    let invalid_empty =
        json!([{"role":"model","parts":[{"functionCall":{"name":"empty","args":{"extra":true}}}]}]);
    assert!(set.mapping(&invalid_empty).is_err());
}

#[test]
fn antigravity_native_terminal_usage_ambiguity_is_not_hidden() {
    let mut reducer = Reducer::default();
    reducer.push(format!("data: {}\n\n",json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"text"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":2,"totalTokenCount":15}}})).as_bytes()).unwrap();
    assert!(reducer.finish().is_err());
}

fn golden() -> Value {
    let mut original: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/antigravity-golden-v1.json"
    ))
    .unwrap();
    // Keep the four frozen source vectors byte-for-byte unchanged; the reviewed
    // omitted-args extension is an independently exported synthetic fifth vector.
    original["vectors"].as_array_mut().unwrap().push(
        serde_json::from_str(include_str!(
            "../../tests/fixtures/antigravity-parameterless-v1.json"
        ))
        .unwrap(),
    );
    original
}

#[test]
fn antigravity_final_v1_goldens_decode_and_reencode_without_native_mutation() {
    for vector in golden()["vectors"].as_array().unwrap() {
        let binding: replay::Binding = serde_json::from_value(vector["binding"].clone()).unwrap();
        let envelope = replay::decode(&vector["projected_blocks"], &binding).unwrap();
        assert_eq!(envelope, vector["envelope"], "{}", vector["name"]);
        let state = replay::State::from_envelope(&envelope).unwrap();
        let blocks = replay::encode(
            &vector["native_contents"],
            &binding,
            &state,
            envelope["checkpoint_stop_reason"].as_str().unwrap(),
            vector["thought_display"].as_str().unwrap(),
            &envelope["tool_mapping"],
        )
        .unwrap();
        assert_eq!(replay::decode(&blocks, &binding).unwrap(), envelope);
        assert_eq!(
            replay::digest_input(&blocks, &envelope).unwrap(),
            vector["canonical_digest_input_utf8"]
                .as_str()
                .unwrap()
                .as_bytes()
        );
    }
}

#[test]
fn antigravity_final_v1_negative_vectors_fail_closed() {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/antigravity-negative-v1.json"
    ))
    .unwrap();
    for vector in vectors["vectors"].as_array().unwrap() {
        let input = &vector["input"];
        let binding: replay::Binding = serde_json::from_value(input["binding"].clone()).unwrap();
        let rejected = match vector["operation"].as_str().unwrap() {
            "decode_turn" => replay::decode(&input["blocks"], &binding).is_err(),
            "decode_history" => replay::decode_history(&input["turns"], &binding).is_err(),
            "project_turn" => replay::mapping(&input["contents"], Some(&input["tool_projections"]))
                .and_then(|mapping| {
                    encode_default(
                        &input["contents"],
                        &binding,
                        input["thought_display"].as_str().unwrap(),
                        &mapping,
                    )
                })
                .is_err(),
            other => panic!("unhandled frozen vector {other}"),
        };
        assert!(rejected, "{}", vector["name"]);
    }
}

#[test]
fn antigravity_replay_rejects_missing_carriers_and_scope_changes() {
    let vectors = golden();
    let vector = &vectors["vectors"][0];
    let binding: replay::Binding = serde_json::from_value(vector["binding"].clone()).unwrap();
    let blocks = vector["projected_blocks"].clone();
    assert!(replay::decode(&json!([blocks[1]]), &binding).is_err());
    assert!(replay::decode(&json!([blocks[0], blocks[0], blocks[1]]), &binding).is_err());
    for key in ["native_model", "project_id", "profile_id"] {
        let mut wrong = vector["binding"].clone();
        wrong[key] = json!("different");
        assert!(replay::decode(&blocks, &serde_json::from_value(wrong).unwrap()).is_err());
    }
}

#[test]
fn antigravity_json_parser_and_argument_codec_enforce_bounds_and_uniqueness() {
    for raw in [r#"{"x":1,"x":2}"#, r#"[NaN]"#, r#"[Infinity]"#] {
        assert!(replay::parse_json(raw.as_bytes(), 1024).is_err());
    }
    let deep = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert!(replay::parse_json(deep.as_bytes(), 1024).is_err());
    assert!(replay::parse_json(b"{}", 1).is_err());
    for raw in ["9007199254740992", "1e100"] {
        assert!(replay::canonical(&serde_json::from_str::<Value>(raw).unwrap()).is_err());
    }
    assert_eq!(
        replay::canonical(&json!(-0.0)).unwrap(),
        replay::canonical(&json!(0)).unwrap()
    );
    assert_eq!(
        replay::canonical(&json!(1.0)).unwrap(),
        replay::canonical(&json!(1)).unwrap()
    );
}

#[test]
fn antigravity_local_tool_ids_are_unique_despite_reused_native_ids() {
    let vectors = golden();
    let contents = &vectors["vectors"][3]["native_contents"];
    let first = replay::mapping(contents, None).unwrap();
    let second = replay::mapping(contents, None).unwrap();
    let mut ids = std::collections::HashSet::new();
    for entry in first
        .as_array()
        .unwrap()
        .iter()
        .chain(second.as_array().unwrap())
    {
        assert!(ids.insert(entry["local_id"].clone().to_string()));
        assert_ne!(entry["local_id"], entry["native_id"]);
    }
}

#[test]
fn antigravity_strips_claude_code_billing_and_sdk_headers_and_consolidates_agy_system_parts() {
    for sdk_header in [
        "You are Claude Code, Anthropic's official CLI for Claude.",
        "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
        "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
    ] {
        let mut request = request_value();
        request["system"] = json!([
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.270.dd6; cc_entrypoint=sdk-cli;"},
            {"type":"text","text":sdk_header,"cache_control":{"type":"ephemeral"}},
            {"type":"text","text":"\nYou are an interactive agent.","cache_control":{"type":"ephemeral"}}
        ]);
        request["messages"] = json!([
            {"role":"system","content":"# Environment\n - Primary working directory: /workspace"},
            {"role":"user","content":"Reply with OK."}
        ]);

        let prepared = prepare(&serde_json::from_value(request).unwrap(), &binding()).unwrap();
        assert_eq!(
            prepared.wire["request"]["systemInstruction"],
            json!({
                "role":"user",
                "parts":[{"text":"\nYou are an interactive agent.\n\n# Environment\n - Primary working directory: /workspace"}]
            })
        );

        // Pre-flattened single-string system with leading billing + SDK headers.
        let mut flattened = request_value();
        flattened["system"] = json!(format!(
            "x-anthropic-billing-header: cc_version=2.1.270.dd6\n\n{sdk_header}\n\n\nYou are an interactive agent."
        ));
        let flattened_prepared =
            prepare(&serde_json::from_value(flattened).unwrap(), &binding()).unwrap();
        assert_eq!(
            flattened_prepared.wire["request"]["systemInstruction"],
            json!({
                "role":"user",
                "parts":[{"text":"\nYou are an interactive agent."}]
            })
        );

        // System consisting solely of billing + SDK headers omits systemInstruction entirely.
        let mut headers_only = request_value();
        headers_only["system"] = json!([
            {"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.270.dd6"},
            {"type":"text","text":sdk_header}
        ]);
        let omitted = prepare(&serde_json::from_value(headers_only).unwrap(), &binding()).unwrap();
        assert!(omitted.wire["request"].get("systemInstruction").is_none());
    }
}

#[test]
fn antigravity_opus_5_5_and_carrierless_prefix_turns_and_coalesced_text_blocks() {
    use super::antigravity::request::{CountInputs, estimate};
    use crate::MessagesRequest;

    let opus_binding = replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "claude-opus-5-5-low".into(),
    };
    let lineage = default_lineage();
    let signed_turn = encode_turn(
        &json!([{"role":"model","parts":[{"thought":true,"text":"thinking","thoughtSignature":"sig-1"},{"text":"Turn 2 reply."}]}]),
        &opus_binding,
        &lineage,
        0,
        "full",
        &json!([]),
    )
    .unwrap();

    // Carrier-less prefix (from mid-session /model switch) + active signed turn
    let mut switched = request_value();
    switched["model"] = json!("antigravity/claude-opus-5-5-low");
    switched["messages"] = json!([
        {"role":"user","content":"Remember token ALPHA."},
        {"role":"assistant","content":[
            {"type":"text","text":"Checking tool."},
            {"type":"tool_use","id":"toolu_prefix_1","name":"Read","input":{"file_path":"/tmp/a.txt"}}
        ]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_prefix_1","content":"ALPHA"}
        ]},
        {"role":"assistant","content":[
            {"type":"text","text":"Token ALPHA recorded."}
        ]},
        {"role":"user","content":"Now switch model."},
        {"role":"assistant","content":signed_turn},
        {"role":"user","content":"What was the token?"}
    ]);

    let body: MessagesRequest = serde_json::from_value(switched).unwrap();
    let mut prepared = prepare(&body, &opus_binding).unwrap();
    let mut ids = [execution_id(1)].into_iter();
    prepared
        .finalize_metadata(1_799_999_999_456, || ids.next().unwrap())
        .unwrap();
    assert_eq!(
        prepared.wire["request"]["labels"]["model_enum"],
        json!("MODEL_PLACEHOLDER_M400")
    );
    let contents = prepared.wire["request"]["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 7);
    assert_eq!(
        contents[0],
        json!({"role":"user","parts":[{"text":"Remember token ALPHA."}]})
    );
    assert_eq!(
        contents[1],
        json!({"role":"model","parts":[
            {"text":"Checking tool."},
            {"text":"[tool_use Read(id=toolu_prefix_1): {\"file_path\":\"/tmp/a.txt\"}]"}
        ]})
    );
    assert_eq!(
        contents[2],
        json!({"role":"user","parts":[
            {"text":"[tool_result (toolu_prefix_1): ALPHA]"}
        ]})
    );
    assert_eq!(
        contents[3],
        json!({"role":"model","parts":[{"text":"Token ALPHA recorded."}]})
    );
    assert_eq!(
        contents[4],
        json!({"role":"user","parts":[{"text":"Now switch model."}]})
    );
    assert_eq!(contents[5]["role"], json!("model"));
    assert_eq!(
        contents[6],
        json!({"role":"user","parts":[{"text":"What was the token?"}]})
    );

    let counted = estimate(
        &body,
        CountInputs {
            serving_scope: None,
            configured_project: Some("project"),
        },
    )
    .unwrap();
    assert!(counted > 0);
}

#[test]
fn antigravity_canonicalizes_client_normalized_tool_inputs_and_accepts_global_cache_scope_and_xhigh_effort()
 {
    let opus_binding = replay::Binding {
        profile_id: "profile".into(),
        project_id: "project".into(),
        native_model: "claude-opus-5-5-medium".into(),
    };
    let native_call = json!([{"role":"model","parts":[
        {"thought":true,"text":"Running git pull.","thoughtSignature":"sig-bash"},
        {"functionCall":{
            "name":"Bash",
            "id":"toolu_vrtx_014vTPhzW5gH8g73V1H3aPPh",
            "args":{
                "command":"cd /data/chrisliu/DataFactory && git pull --ff-only -q origin master 2>&1 | tail -2; git log -1 --oneline",
                "description":"Pull latest master"
            }
        }}
    ]}]);
    let mapping = replay::mapping(&native_call, None).unwrap();
    let mut blocks = encode_turn(
        &native_call,
        &opus_binding,
        &default_lineage(),
        0,
        "full",
        &mapping,
    )
    .unwrap();
    let local_id = blocks[1]["id"].as_str().unwrap().to_owned();

    // Simulate client-side manipulations (Claude Code Bash updatedInput, cleared thinking text,
    // extra cache_control / metadata fields).
    blocks[0]["thinking"] = json!("");
    blocks[1]["input"]["command"] =
        json!("git pull --ff-only -q origin master 2>&1 | tail -2; git log -1 --oneline");
    assert!(
        replay::decode(&blocks, &opus_binding).is_err(),
        "raw decode remains strict on uncanonicalized blocks"
    );

    let mut request = request_value();
    request["model"] = json!("antigravity/claude-opus-5-5-medium");
    request["output_config"] = json!({"effort":"xhigh"});
    request["metadata"] = json!({"user_id":"u1","session_id":"s1"});
    request["service_tier"] = json!("auto");
    request["system"] = json!([
        {"type":"text","text":"Global system instruction","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global","future_hint":true}}
    ]);
    request["messages"] = json!([
        {"role":"user","content":"Investigate why VE TEST PostgreSQL OOMed"},
        {"role":"assistant","content":blocks.clone()},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":local_id,"content":"96672b397 Merge branch"}
        ]}
    ]);
    let prepared = prepare(
        &serde_json::from_value(request.clone()).unwrap(),
        &opus_binding,
    )
    .unwrap();
    assert_eq!(
        prepared.wire["request"]["contents"][1]["parts"][1]["functionCall"]["args"]["command"],
        json!(
            "cd /data/chrisliu/DataFactory && git pull --ff-only -q origin master 2>&1 | tail -2; git log -1 --oneline"
        )
    );

    // Switching mid-session from Opus 5.5 to Gemini 4 Argon low after an interrupted tool_use
    // with Claude Code's `thinking: {"type": "enabled", "budget_tokens": 31999}` succeeds by
    // folding prior-model turns and the trailing prefix tool_result into prefix_messages.
    let mut switched = request.clone();
    switched["model"] = json!("antigravity/gemini-4-argon-low");
    switched["thinking"] = json!({"type":"enabled","budget_tokens":31999});
    switched["messages"][2]["content"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"type":"text","text":"[Request interrupted by user]"}),
            json!({"type":"text","text":"continue"}),
        ]);
    let argon_prepared = prepare(&serde_json::from_value(switched).unwrap(), &binding()).unwrap();
    assert_eq!(argon_prepared.wire["model"], json!("gemini-4-argon-low"));
    assert_eq!(
        argon_prepared.wire["request"]["contents"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Tampering with the carrier signature itself still fails closed in prepare.
    let mut tampered_sig = request;
    tampered_sig["messages"][1]["content"][0]["signature"] =
        json!(format!("{}corrupted", replay::PREFIX));
    assert!(
        prepare(
            &serde_json::from_value(tampered_sig).unwrap(),
            &opus_binding
        )
        .is_err()
    );
}
