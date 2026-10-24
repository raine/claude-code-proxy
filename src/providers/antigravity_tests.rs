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

#[test]
fn antigravity_absent_native_args_project_empty_without_changing_replay() {
    let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"empty"},"thoughtSignature":"untouched"}]}]);
    let set = ToolSet::compile(
        &json!([{"name":"empty","input_schema":{"type":"object","additionalProperties":false}}]),
    )
    .unwrap();
    let mapping = set.mapping(&contents).unwrap();
    let blocks = replay::encode(&contents, &binding(), "19", "full", &mapping).unwrap();
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
        let blocks = replay::encode(&contents, &binding(), "19", "full", &mapping).unwrap();
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
    let blocks = replay::encode(&contents, &binding(), "19", "full", &json!([])).unwrap();
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
        json!({"role":"assistant","content":blocks}),
        json!({"role":"user","content":"<system-reminder>ordinary user text</system-reminder>"}),
    ]);
    let prepared = prepare(&serde_json::from_value(distinct).unwrap(), &binding()).unwrap();
    assert_eq!(
        prepared.wire["request"]["contents"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        prepared.wire["request"]["systemInstruction"]["parts"],
        json!([{"text":"privileged"}])
    );
    assert_eq!(prepared.wire["request"]["contents"][3]["role"], "user");
}

#[test]
fn antigravity_count_estimate_is_independent_of_thought_display_and_fragmentation() {
    let contents = json!([{"role":"model","parts":[{"thought":true,"text":"retained hidden thought ".repeat(200)},{"text":"answer","thoughtSignature":"opaque"}]}]);
    let mut counts = Vec::new();
    for display in ["full", "omitted"] {
        let blocks = replay::encode(&contents, &binding(), "19", display, &json!([])).unwrap();
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
        let blocks = replay::encode(&contents, &binding(), "19", display, &json!([])).unwrap();
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
                let blocks = replay::encode(
                    &native,
                    &scope,
                    "19",
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
    let blocks = replay::encode(&contents, &binding(), "19", "full", &mapping).unwrap();
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
    assert_eq!(properties["lower"]["minimum"], 2);
    assert_eq!(properties["upper"]["maximum"], -2);
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
            set.declarations[0]["parameters"]["properties"]["limit"]["minimum"],
            1
        );
        for args in [json!({"limit":0}), json!({"limit":1,"extra":true})] {
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
    let blocks = replay::encode(&contents, &binding(), "1", "omitted", &mapping).unwrap();
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
    replay::session_id(prepared.wire["request"]["sessionId"].as_str().unwrap()).unwrap();
}

#[test]
fn antigravity_request_system_parts_keep_privilege_and_order() {
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
        prepared.wire["request"]["systemInstruction"]["parts"],
        json!([{"text":"top"},{"text":"first"},{"text":"second"}])
    );
    assert_eq!(
        prepared.wire["request"]["contents"],
        json!([{"role":"user","parts":[{"text":"hello"}]}])
    );
    request["messages"] = json!([{"role":"system","content":"only system"}]);
    assert!(prepare(&serde_json::from_value(request).unwrap(), &binding()).is_err());
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
    for session in [
        "0",
        "-0",
        "+1",
        "01",
        "-01",
        "9223372036854775808",
        "-9223372036854775809",
    ] {
        assert!(replay::session_id(session).is_err());
    }
    for session in ["1", "-1", "9223372036854775807", "-9223372036854775808"] {
        assert_eq!(replay::session_id(session).unwrap(), session);
    }
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
    let blocks = replay::encode(&contents, &binding(), "123", "full", &mapping).unwrap();
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
    assert!(set.declarations[1].get("parameters").is_none());
    let contents = json!([{"role":"model","parts":[{"functionCall":{"name":"free","args":{"json":"{\"x\":[1,true]}"}}},{"functionCall":{"name":"empty","args":{}}}]}]);
    let map = set.mapping(&contents).unwrap();
    let blocks = replay::encode(&contents, &binding(), "1", "full", &map).unwrap();
    assert_eq!(blocks[1]["input"], json!({"x":[1,true]}));
    assert_eq!(blocks[2]["input"], json!({}));
    let invalid = json!([{"role":"model","parts":[{"functionCall":{"name":"free","args":{"json":"{}","extra":true}}}]}]);
    assert!(set.mapping(&invalid).is_err());
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
        let blocks = replay::encode(
            &vector["native_contents"],
            &binding,
            vector["session_id"].as_str().unwrap(),
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
                    replay::encode(
                        &input["contents"],
                        &binding,
                        input["session_id"].as_str().unwrap(),
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
