//! Observed tool-schema shapes with synthetic names and text, not private input replay.
use super::antigravity::{replay::Binding, request::prepare};
use serde_json::{Value, json};

fn synthetic_caller_schema(native: &Value) -> Value {
    let source = native.as_object().unwrap();
    assert!(source.keys().all(|key| matches!(
        key.as_str(),
        "type"
            | "description"
            | "properties"
            | "items"
            | "required"
            | "enum"
            | "default"
            | "format"
            | "pattern"
            | "minimum"
            | "maximum"
            | "minLength"
            | "maxLength"
            | "minItems"
            | "maxItems"
    )));
    let kind = source["type"].as_str().unwrap();
    assert!(matches!(
        kind,
        "OBJECT" | "ARRAY" | "STRING" | "INTEGER" | "NUMBER" | "BOOLEAN"
    ));
    let mut caller = source.clone();
    caller.insert("type".into(), json!(kind.to_ascii_lowercase()));
    if let Some(properties) = source.get("properties") {
        caller.insert(
            "properties".into(),
            Value::Object(
                properties
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(name, schema)| (name.clone(), synthetic_caller_schema(schema)))
                    .collect(),
            ),
        );
    }
    if let Some(items) = source.get("items") {
        caller.insert("items".into(), synthetic_caller_schema(items));
    }
    for key in ["minLength", "maxLength", "minItems", "maxItems"] {
        if let Some(value) = source.get(key) {
            let limit = value.as_str().unwrap().parse::<u64>().unwrap();
            assert!(limit <= 9_007_199_254_740_991);
            caller.insert(key.into(), json!(limit));
        }
    }
    if kind == "OBJECT"
        && source
            .get("properties")
            .and_then(Value::as_object)
            .is_none_or(|properties| properties.is_empty())
    {
        caller.insert("additionalProperties".into(), json!(false));
    }
    Value::Object(caller)
}

#[test]
fn antigravity_observed_tools_match_complete_synthetic_groups() {
    let expected: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/antigravity-observed-tool-schemas.json"
    ))
    .unwrap();
    let groups = expected.as_array().unwrap();
    assert_eq!(groups.len(), 12);
    let tools: Vec<Value> = groups
        .iter()
        .map(|group| {
            assert_eq!(group.as_object().unwrap().len(), 1);
            let declarations = group["functionDeclarations"].as_array().unwrap();
            assert_eq!(declarations.len(), 1);
            let mut tool = declarations[0].as_object().unwrap().clone();
            assert!(
                tool.keys()
                    .all(|key| matches!(key.as_str(), "name" | "description" | "parameters"))
            );
            let parameters = tool.remove("parameters").unwrap();
            tool.insert("input_schema".into(), synthetic_caller_schema(&parameters));
            Value::Object(tool)
        })
        .collect();
    let request = serde_json::from_value(json!({
        "model":"antigravity/gemini-4-argon-low",
        "max_tokens":512,
        "messages":[{"role":"user","content":"synthetic captured-schema request"}],
        "tools":tools
    }))
    .unwrap();
    let binding = Binding {
        profile_id: "synthetic-profile".into(),
        project_id: "synthetic-project".into(),
        native_model: "gemini-4-argon-low".into(),
    };
    let prepared = prepare(&request, &binding).unwrap();
    let actual = &prepared.wire["request"]["tools"];
    assert_eq!(
        actual[3]["functionDeclarations"][0]["parameters"]["properties"]["field_0001"]["minLength"],
        expected[3]["functionDeclarations"][0]["parameters"]["properties"]["field_0001"]["minLength"],
        "observed int64 schema limits require quoted serialization"
    );
    assert_eq!(actual, &expected, "complete synthetic tool groups differ");
}
