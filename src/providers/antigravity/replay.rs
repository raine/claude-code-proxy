//! Exact frozen v1 transcript transport. The digest detects inconsistency, not forgery.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub(crate) type Result<T> = std::result::Result<T, &'static str>;
pub const PREFIX: &str = "ccp:antigravity:v1:";
pub const MAX_DECODED: usize = 8 * 1024 * 1024;
pub const MAX_ENCODED: usize = 12 * 1024 * 1024;
pub const MAX_ARGUMENT: usize = 1024 * 1024;
pub const MAX_DEPTH: usize = 64;

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub profile_id: String,
    pub project_id: String,
    pub native_model: String,
}

pub(crate) fn keys(value: &Value, allowed: &[&str], required: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or("expected object")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || required.iter().any(|key| !object.contains_key(*key))
    {
        return Err("unsupported or missing fields");
    }
    Ok(())
}

pub(crate) fn nonempty(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("expected nonempty string")
}

// Check depth before deserializing; the visitor also rejects duplicate keys at
// every level (serde_json::Value's default parser otherwise keeps the last one).
pub(crate) fn parse_json(raw: &[u8], max: usize) -> Result<Value> {
    if raw.len() > max {
        return Err("JSON byte limit");
    }
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut escape = false;
    for byte in raw {
        if quoted {
            if escape {
                escape = false;
            } else if *byte == b'\\' {
                escape = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else if *byte == b'"' {
            quoted = true;
        } else if matches!(byte, b'[' | b'{') {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err("JSON depth limit");
            }
        } else if matches!(byte, b']' | b'}') {
            depth = depth.saturating_sub(1);
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let value = StrictValue::deserialize(&mut decoder).map_err(|_| "invalid JSON")?;
    decoder.end().map_err(|_| "invalid JSON")?;
    Ok(value.0)
}

struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: de::Deserializer<'de>>(decoder: D) -> std::result::Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("unique-key JSON")
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(value)))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(value)))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|v| StrictValue(Value::Number(v)))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(value)))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, StrictValue(value))) =
                    map.next_entry::<String, StrictValue>()?
                {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        decoder.deserialize_any(StrictVisitor)
    }
}

pub(crate) fn canonical(value: &Value) -> Result<Vec<u8>> {
    fn tagged(value: &Value, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err("JSON depth limit");
        }
        Ok(match value {
            Value::Null => json!(["null"]),
            Value::Bool(v) => json!(["bool", v]),
            Value::String(v) => json!(["string", v]),
            Value::Number(v) => {
                let n = v.as_f64().ok_or("invalid number")?;
                if !n.is_finite() || (n.fract() == 0.0 && n.abs() > 9007199254740991.0) {
                    return Err("unsafe number");
                }
                let bits = if n == 0.0 { 0 } else { n.to_bits() };
                json!(["number", format!("{bits:016x}")])
            }
            Value::Array(v) => json!([
                "array",
                v.iter()
                    .map(|v| tagged(v, depth + 1))
                    .collect::<Result<Vec<_>>>()?
            ]),
            Value::Object(v) => {
                let mut pairs = v.iter().collect::<Vec<_>>();
                pairs.sort_by(|a, b| a.0.cmp(b.0));
                json!([
                    "object",
                    pairs
                        .into_iter()
                        .map(|(k, v)| tagged(v, depth + 1).map(|v| json!([k, v])))
                        .collect::<Result<Vec<_>>>()?
                ])
            }
        })
    }
    serde_json::to_vec(&tagged(value, 0)?).map_err(|_| "JSON serialization failed")
}

pub(crate) fn session_id(value: &str) -> Result<&str> {
    if value.len() > 20 {
        return Err("invalid session ID");
    }
    let n = value.parse::<i64>().map_err(|_| "invalid session ID")?;
    if n == 0 || n.to_string() != value {
        return Err("invalid session ID");
    }
    Ok(value)
}

pub(crate) fn mint_session() -> String {
    ((rand::random::<u64>() >> 1).max(1)).to_string()
}

fn display(value: &str) -> Result<()> {
    if matches!(value, "full" | "omitted") {
        Ok(())
    } else {
        Err("invalid thought display")
    }
}

type Call<'a> = (usize, usize, &'a Value);

/// Omission means an empty completed call in the caller projection only. Never
/// insert this default into the saved native content/signature structure.
pub(crate) fn call_arguments(call: &Value) -> Result<&Value> {
    static EMPTY: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| json!({}));
    if call.get("partialArgs").is_some() || call.get("willContinue").is_some() {
        return Err("incremental native function calls are unsupported");
    }
    match call.get("args") {
        None => Ok(&EMPTY),
        Some(value) if value.is_object() => Ok(value),
        Some(_) => Err("invalid native call arguments"),
    }
}
pub(crate) fn calls(contents: &Value) -> Result<Vec<Call<'_>>> {
    let contents = contents
        .as_array()
        .filter(|v| v.len() <= 512)
        .ok_or("native content limit/type")?;
    let mut calls = Vec::new();
    let mut idless = HashSet::new();
    let mut part_count = 0;
    for (ci, content) in contents.iter().enumerate() {
        if content["role"] != "model" {
            return Err("invalid native content role");
        }
        let parts = content["parts"].as_array().ok_or("invalid native parts")?;
        part_count += parts.len();
        if part_count > 4096 {
            return Err("native part limit");
        }
        for (pi, part) in parts.iter().enumerate() {
            if !part.is_object() || part.get("text").is_some_and(|v| !v.is_string()) {
                return Err("invalid native part");
            }
            let Some(call) = part.get("functionCall") else {
                continue;
            };
            let name = nonempty(&call["name"])?;
            call_arguments(call)?;
            if let Some(id) = call.get("id") {
                nonempty(id)?;
            } else if !idless.insert(name) {
                return Err("ambiguous same-name ID-less parallel calls");
            }
            calls.push((ci, pi, call));
            if calls.len() > 128 {
                return Err("tool count limit");
            }
        }
    }
    Ok(calls)
}

fn pointer(path: &Value) -> Result<Vec<String>> {
    let path = path
        .as_str()
        .filter(|s| s.starts_with('/') && s.len() <= 1024)
        .ok_or("invalid codec path")?;
    let mut tokens = Vec::new();
    for token in path[1..].split('/') {
        let mut chars = token.chars();
        let mut decoded = String::new();
        while let Some(c) = chars.next() {
            decoded.push(if c != '~' {
                c
            } else {
                match chars.next() {
                    Some('0') => '~',
                    Some('1') => '/',
                    _ => return Err("invalid codec escape"),
                }
            });
        }
        tokens.push(decoded);
    }
    if tokens.len() > MAX_DEPTH {
        return Err("codec path depth limit");
    }
    Ok(tokens)
}

fn decode_leaf(value: &Value) -> Result<Value> {
    let raw = value.as_str().ok_or("codec leaf must be a JSON string")?;
    let value = parse_json(raw.as_bytes(), MAX_ARGUMENT)?;
    canonical(&value)?;
    Ok(value)
}

pub(crate) fn decode_arguments(args: &Value, paths: &Value, root: &Value) -> Result<Value> {
    let paths = paths
        .as_array()
        .filter(|v| v.len() <= 64)
        .ok_or("invalid codec paths")?;
    if !args.is_object() {
        return Err("arguments must be object");
    }
    let result = if !root.is_null() {
        let key = nonempty(root)?;
        if key.len() > 256
            || !paths.is_empty()
            || args.as_object().unwrap().len() != 1
            || args.get(key).is_none()
        {
            return Err("invalid root wrapper");
        }
        let result = decode_leaf(&args[key])?;
        if !result.is_object() {
            return Err("root codec must decode object");
        }
        result
    } else {
        let paths = paths.iter().map(pointer).collect::<Result<Vec<_>>>()?;
        for (i, left) in paths.iter().enumerate() {
            if paths[i + 1..]
                .iter()
                .any(|right| left.starts_with(right) || right.starts_with(left))
            {
                return Err("overlapping codec paths");
            }
        }
        let mut result = args.clone();
        for tokens in paths {
            let mut current = &mut result;
            for token in &tokens {
                current = match current {
                    Value::Object(object) => object.get_mut(token).ok_or("missing codec key")?,
                    Value::Array(array) => {
                        let index = token
                            .parse::<usize>()
                            .map_err(|_| "invalid codec array index")?;
                        if token.len() > 10 || index.to_string() != *token {
                            return Err("invalid codec array index");
                        }
                        array
                            .get_mut(index)
                            .ok_or("codec array index out of range")?
                    }
                    _ => return Err("codec path traverses scalar"),
                };
            }
            *current = decode_leaf(current)?;
        }
        result
    };
    canonical(&result)?;
    if serde_json::to_vec(&result)
        .map_err(|_| "invalid arguments")?
        .len()
        > MAX_ARGUMENT
    {
        return Err("argument byte limit");
    }
    Ok(result)
}

pub(crate) fn mapping(contents: &Value, recipes: Option<&Value>) -> Result<Value> {
    let calls = calls(contents)?;
    let recipes = recipes
        .map(|v| {
            v.as_array()
                .filter(|v| v.len() == calls.len())
                .ok_or("recipe count mismatch")
        })
        .transpose()?;
    let turn = uuid::Uuid::new_v4().simple().to_string();
    let mut mappings = Vec::new();
    for (i, (ci, pi, call)) in calls.iter().enumerate() {
        let empty = json!({});
        let recipe = recipes.map(|v| &v[i]).unwrap_or(&empty);
        keys(
            recipe,
            &["local_id", "caller_name", "codec_paths", "root_wrapper"],
            &[],
        )?;
        let mut entry = json!({
            "local_id": recipe.get("local_id").cloned().unwrap_or_else(|| json!(format!("toolu_{turn}_{i:04}"))),
            "caller_name": recipe.get("caller_name").unwrap_or(&call["name"]),
            "content_index": ci, "part_index": pi, "native_id_present": call.get("id").is_some(),
            "codec_paths": recipe.get("codec_paths").cloned().unwrap_or_else(|| json!([])),
            "root_wrapper": recipe.get("root_wrapper").cloned().unwrap_or(Value::Null)
        });
        if let Some(id) = call.get("id") {
            entry["native_id"] = id.clone();
        }
        mappings.push(entry);
    }
    Ok(json!(mappings))
}

pub(crate) fn projection(
    contents: &Value,
    thought_display: &str,
    mapping: &Value,
) -> Result<Value> {
    display(thought_display)?;
    let calls = calls(contents)?;
    let entries = mapping
        .as_array()
        .filter(|v| v.len() == calls.len())
        .ok_or("mapping count mismatch")?;
    let mut ids = HashSet::new();
    let mut projected_calls = std::collections::BTreeMap::new();
    for ((ci, pi, call), entry) in calls.iter().zip(entries) {
        let mut required = vec![
            "local_id",
            "caller_name",
            "content_index",
            "part_index",
            "native_id_present",
            "codec_paths",
            "root_wrapper",
        ];
        if call.get("id").is_some() {
            required.push("native_id");
        }
        keys(entry, &required, &required)?;
        if entry["content_index"].as_u64() != Some(*ci as u64)
            || entry["part_index"].as_u64() != Some(*pi as u64)
            || entry["native_id_present"].as_bool() != Some(call.get("id").is_some())
            || entry.get("native_id") != call.get("id")
        {
            return Err("mapping native position/ID mismatch");
        }
        let id = nonempty(&entry["local_id"])?;
        if id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || !ids.insert(id)
        {
            return Err("invalid/duplicate local ID");
        }
        if nonempty(&entry["caller_name"])?.len() > 1024 {
            return Err("caller name limit");
        }
        let args = decode_arguments(
            call_arguments(call)?,
            &entry["codec_paths"],
            &entry["root_wrapper"],
        )?;
        projected_calls.insert(
            (*ci, *pi),
            json!({"type":"tool_use", "id":id, "name":entry["caller_name"], "input":args}),
        );
    }
    let mut thoughts = String::new();
    let mut blocks = Vec::new();
    for (ci, content) in contents.as_array().unwrap().iter().enumerate() {
        for (pi, part) in content["parts"].as_array().unwrap().iter().enumerate() {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if part["thought"] == true {
                    thoughts.push_str(text);
                } else if !text.is_empty() {
                    blocks.push(json!({"type":"text", "text":text}));
                }
            }
            if let Some(block) = projected_calls.remove(&(ci, pi)) {
                blocks.push(block);
            }
        }
    }
    blocks.insert(0, json!({"type":"thinking", "thinking":if thought_display == "omitted" { "" } else { &thoughts }}));
    Ok(json!(blocks))
}

/// Count bounded projected semantic bytes without serializing a large argument
/// into a second allocation. Signatures and cache hints are transport metadata.
pub(crate) fn projected_block_bytes(block: &Value) -> Result<usize> {
    match block["type"].as_str() {
        Some("text") => block["text"]
            .as_str()
            .map(str::len)
            .ok_or("invalid display text"),
        Some("thinking") => block["thinking"]
            .as_str()
            .map(str::len)
            .ok_or("invalid display thinking"),
        Some("tool_use") => {
            struct Count(usize);
            impl std::io::Write for Count {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    if self.0.saturating_add(bytes.len()) > MAX_ARGUMENT {
                        return Err(std::io::Error::other("argument byte limit"));
                    }
                    self.0 += bytes.len();
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            if !block["input"].is_object() {
                return Err("invalid tool input");
            }
            let mut count = Count(0);
            serde_json::to_writer(&mut count, &block["input"])
                .map_err(|_| "argument byte limit")?;
            Ok(count.0 + nonempty(&block["name"])?.len() + nonempty(&block["id"])?.len())
        }
        _ => Err("unsupported projection block"),
    }
}

pub(crate) fn projection_values(blocks: &Value) -> Result<Value> {
    let blocks = blocks
        .as_array()
        .filter(|b| b.len() <= 4097)
        .ok_or("invalid projection blocks")?;
    let mut result: Vec<Value> = Vec::new();
    let mut semantic_bytes = 0_usize;
    for block in blocks {
        semantic_bytes = semantic_bytes
            .checked_add(projected_block_bytes(block)?)
            .ok_or("projection byte limit")?;
        if semantic_bytes > MAX_DECODED {
            return Err("projection byte limit");
        }
        let kind = block["type"].as_str().ok_or("invalid block type")?;
        let mut allowed = vec!["type", "signature", "cache_control"];
        let item = match kind {
            "thinking" | "text" => {
                allowed.push(kind);
                let text = block[kind].as_str().ok_or("invalid display text")?;
                json!({"type":kind, kind:text})
            }
            "tool_use" => {
                allowed.extend(["id", "name", "input"]);
                nonempty(&block["id"])?;
                nonempty(&block["name"])?;
                if !block["input"].is_object() {
                    return Err("invalid tool input");
                }
                json!({"type":kind, "id":block["id"], "name":block["name"], "input":block["input"]})
            }
            _ => return Err("unsupported projection block"),
        };
        keys(block, &allowed, &["type"])?;
        if kind == "text" && result.last().is_some_and(|b| b["type"] == "text") {
            let last = result.last_mut().unwrap();
            let Value::String(text) = &mut last["text"] else {
                unreachable!("validated text block")
            };
            text.push_str(item["text"].as_str().unwrap());
        } else {
            result.push(item);
        }
    }
    Ok(json!(result))
}

pub(crate) fn digest_input(blocks: &Value, envelope: &Value) -> Result<Vec<u8>> {
    let mut binding = Map::new();
    for key in [
        "profile_id",
        "project_id",
        "native_model",
        "session_id",
        "thought_display",
        "tool_mapping",
    ] {
        binding.insert(
            key.to_string(),
            envelope.get(key).ok_or("missing binding")?.clone(),
        );
    }
    canonical(&json!({"binding":binding, "blocks":projection_values(blocks)?}))
}

fn digest(blocks: &Value, envelope: &Value) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(digest_input(blocks, envelope)?)
    ))
}

pub(crate) fn encode(
    contents: &Value,
    binding: &Binding,
    session: &str,
    thought_display: &str,
    mapping: &Value,
) -> Result<Value> {
    session_id(session)?;
    let mut blocks = projection(contents, thought_display, mapping)?;
    let mut envelope = serde_json::to_value(binding).map_err(|_| "invalid binding")?;
    if [
        &binding.profile_id,
        &binding.project_id,
        &binding.native_model,
    ]
    .iter()
    .any(|v| v.is_empty())
    {
        return Err("empty binding");
    }
    let fields = json!({"version":1, "session_id":session, "thought_display":thought_display, "contents":contents, "tool_mapping":mapping});
    envelope
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    envelope["projection_digest"] = json!(digest(&blocks, &envelope)?);
    let raw = serde_json::to_vec(&envelope).map_err(|_| "invalid envelope")?;
    // Same bounded parser as replay; don't emit a carrier we cannot decode.
    parse_json(&raw, MAX_DECODED)?;
    let encoded = URL_SAFE_NO_PAD.encode(raw);
    if encoded.len() > MAX_ENCODED {
        return Err("encoded byte limit");
    }
    blocks[0]["signature"] = json!(format!("{PREFIX}{encoded}"));
    Ok(blocks)
}

pub(crate) fn decode(blocks: &Value, binding: &Binding) -> Result<Value> {
    let list = blocks
        .as_array()
        .filter(|b| !b.is_empty())
        .ok_or("missing carrier")?;
    if list[0]["type"] != "thinking" || list.iter().filter(|b| b["type"] == "thinking").count() != 1
    {
        return Err("missing/duplicate/nonleading carrier");
    }
    let encoded = list[0]["signature"]
        .as_str()
        .and_then(|s| s.strip_prefix(PREFIX))
        .ok_or("missing/foreign carrier")?;
    if encoded.is_empty() || encoded.len() > MAX_ENCODED || encoded.len() * 3 / 4 > MAX_DECODED {
        return Err("carrier byte limit");
    }
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "malformed carrier")?;
    let envelope = parse_json(&raw, MAX_DECODED)?;
    let fields = [
        "version",
        "profile_id",
        "project_id",
        "native_model",
        "session_id",
        "thought_display",
        "contents",
        "tool_mapping",
        "projection_digest",
    ];
    keys(&envelope, &fields, &fields)?;
    if envelope["version"].as_u64() != Some(1) {
        return Err("unsupported carrier version");
    }
    for (key, value) in [
        ("profile_id", &binding.profile_id),
        ("project_id", &binding.project_id),
        ("native_model", &binding.native_model),
    ] {
        if value.is_empty() || envelope[key].as_str() != Some(value) {
            return Err("model/profile/project binding mismatch; start a fresh session");
        }
    }
    session_id(nonempty(&envelope["session_id"])?)?;
    let expected = projection(
        &envelope["contents"],
        nonempty(&envelope["thought_display"])?,
        &envelope["tool_mapping"],
    )?;
    let hash = digest(&expected, &envelope)?;
    if envelope["projection_digest"] != hash || digest(blocks, &envelope)? != hash {
        return Err("projection digest mismatch");
    }
    Ok(envelope)
}

pub(crate) fn decode_history(turns: &Value, binding: &Binding) -> Result<Vec<Value>> {
    let turns = turns
        .as_array()
        .filter(|t| t.len() <= 512)
        .ok_or("history limit/type")?;
    let mut result = Vec::new();
    let mut session = None;
    let mut ids = HashSet::new();
    for turn in turns {
        let envelope = decode(turn, binding)?;
        let current = envelope["session_id"].as_str().unwrap().to_string();
        if session.as_ref().is_some_and(|s| s != &current) {
            return Err("conflicting session IDs");
        }
        session = Some(current);
        for entry in envelope["tool_mapping"].as_array().unwrap() {
            if !ids.insert(entry["local_id"].as_str().unwrap().to_string()) {
                return Err("duplicate local ID across history");
            }
        }
        result.push(envelope);
    }
    Ok(result)
}
