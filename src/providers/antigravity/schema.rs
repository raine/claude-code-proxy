//! Antigravity tool schema bridge modeled on `agy`'s `paramsToSchema` /
//! `convertMapToSchema` (`v1main.Schema` protobuf projection).
//!
//! External MCP and Claude Code tool `input_schema` definitions are projected
//! directly into `v1main.Schema` without stringifying fields into `Codec::Json`
//! wrappers or rejecting turns post-generation on local JSON Schema validation
//! mismatches. Downstream callers (Claude Code / MCP servers) validate tool
//! arguments and return `tool_result` (`is_error: true`) so the model can
//! self-correct on the next turn.
use super::replay::{self, Result, keys, nonempty};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const MAX_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_SCHEMA_NODES: usize = 8192;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_REF_DEPTH: usize = 16;

struct Tool {
    caller: String,
    native: String,
}

pub(crate) struct ToolSet {
    pub declarations: Value,
    tools: Vec<Tool>,
}

impl ToolSet {
    pub fn compile(tools: &Value) -> Result<Self> {
        let tools = tools
            .as_array()
            .filter(|t| t.len() <= 128)
            .ok_or("tools must be an array of at most 128 functions")?;
        let mut names = HashSet::new();
        let mut native_names = HashSet::new();
        let mut compiled = Vec::new();
        let mut declarations = Vec::new();
        for tool in tools {
            keys(
                tool,
                &[
                    "name",
                    "description",
                    "input_schema",
                    "cache_control",
                    "defer_loading",
                    "type",
                    "strict",
                    "eager_input_streaming",
                    "input_examples",
                ],
                &["name", "input_schema"],
            )?;
            if let Some(kind) = tool.get("type")
                && !kind.is_null()
                && kind.as_str() != Some("custom")
            {
                return Err("unsupported tool type");
            }
            super::request::cache_hint(tool.get("cache_control"))?;
            let caller = nonempty(&tool["name"])?;
            if caller == "DeferredToolPlaceholder"
                || tool.get("defer_loading") == Some(&Value::Bool(true))
            {
                continue;
            }
            if caller.len() > 1024 || !names.insert(caller) {
                return Err("invalid/duplicate caller tool name");
            }
            let native = if native_name(caller) {
                caller.to_string()
            } else {
                format!("ccp_{:x}", Sha256::digest(caller.as_bytes()))[..64].to_string()
            };
            if !native_names.insert(native.clone()) {
                return Err("native tool name collision");
            }
            let schema = &tool["input_schema"];
            if serde_json::to_vec(schema)
                .map_err(|_| "invalid schema")?
                .len()
                > MAX_SCHEMA_BYTES
            {
                return Err("schema byte limit");
            }
            let root_obj = schema.as_object().ok_or("schema must be object")?;
            if let Some(root_type) = root_obj.get("type")
                && !root_type_allows_object(root_type)
            {
                return Err("tool input_schema root must be an object");
            }
            let mut nodes = 0;
            validate_tree(schema, 0, &mut nodes)?;
            let mut declaration = json!({"name": native});
            if let Some(description) = tool.get("description")
                && !description.is_null()
            {
                let text = description
                    .as_str()
                    .filter(|v| v.len() <= 32768)
                    .ok_or("invalid tool description")?;
                if !text.is_empty() {
                    declaration["description"] = json!(text);
                }
            }
            let mut visited = HashSet::new();
            let mut budget = ProjectionBudget::default();
            declaration["parameters"] =
                project_schema(schema, schema, &mut visited, 0, true, &mut budget);
            declarations.push(declaration);
            compiled.push(Tool {
                caller: caller.to_string(),
                native,
            });
        }
        Ok(Self {
            declarations: json!(declarations),
            tools: compiled,
        })
    }

    pub fn mapping(&self, contents: &Value) -> Result<Value> {
        let mut recipes = Vec::new();
        for (_, _, call) in replay::calls(contents)? {
            let tool = self
                .tools
                .iter()
                .find(|t| call["name"] == t.native)
                .ok_or("upstream returned unknown tool")?;
            let raw_args = replay::call_arguments(call)?;
            replay::decode_arguments(raw_args, &json!([]), &Value::Null)?;
            recipes.push(json!({
                "caller_name": tool.caller,
                "codec_paths": [],
                "root_wrapper": Value::Null,
            }));
        }
        replay::mapping(contents, Some(&json!(recipes)))
    }
}

fn native_name(name: &str) -> bool {
    name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn root_type_allows_object(value: &Value) -> bool {
    match value {
        Value::String(kind) => kind.eq_ignore_ascii_case("object"),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|k| k.eq_ignore_ascii_case("object")),
        _ => false,
    }
}

fn validate_tree(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if depth > MAX_SCHEMA_DEPTH || *nodes > MAX_SCHEMA_NODES {
        return Err("schema depth/node limit");
    }
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
        Value::Number(n) => {
            if n.as_f64().is_none_or(|f| !f.is_finite()) {
                return Err("invalid schema number");
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                validate_tree(item, depth + 1, nodes)?;
            }
            Ok(())
        }
        Value::Object(fields) => {
            for child in fields.values() {
                validate_tree(child, depth + 1, nodes)?;
            }
            Ok(())
        }
    }
}

#[derive(Clone, Default)]
struct ProjectionBudget {
    nodes: usize,
    strings: usize,
}

impl ProjectionBudget {
    fn string(&mut self, value: &str) -> bool {
        let Some(total) = self.strings.checked_add(value.len()) else {
            return false;
        };
        if total > MAX_SCHEMA_BYTES {
            return false;
        }
        self.strings = total;
        true
    }

    fn visit(&mut self, value: &Value, depth: usize) -> bool {
        self.nodes += 1;
        if depth > MAX_SCHEMA_DEPTH || self.nodes > 4096 {
            return false;
        }
        match value {
            Value::Null | Value::Bool(_) => true,
            Value::Number(_) => number_value(value).is_some(),
            Value::String(text) => self.string(text),
            Value::Array(values) => values.iter().all(|v| self.visit(v, depth + 1)),
            Value::Object(values) => values
                .iter()
                .all(|(key, value)| self.string(key) && self.visit(value, depth + 1)),
        }
    }
}

fn number_value(value: &Value) -> Option<f64> {
    const SAFE: i64 = 9_007_199_254_740_991;
    let number = value.as_number()?;
    if let Some(value) = number.as_i64() {
        if !(-SAFE..=SAFE).contains(&value) {
            return None;
        }
        return Some(value as f64);
    }
    if let Some(value) = number.as_u64() {
        if value > SAFE as u64 {
            return None;
        }
        return Some(value as f64);
    }
    number.as_f64().filter(|value| value.is_finite())
}

fn length_value(value: &Value) -> Option<u64> {
    number_value(value)
        .filter(|value| *value >= 0.0 && *value <= 9_007_199_254_740_991.0 && value.fract() == 0.0)
        .map(|value| value as u64)
}

fn genai_type(kind: &str) -> Option<&'static str> {
    if kind.eq_ignore_ascii_case("string") {
        Some("STRING")
    } else if kind.eq_ignore_ascii_case("number") {
        Some("NUMBER")
    } else if kind.eq_ignore_ascii_case("integer") {
        Some("INTEGER")
    } else if kind.eq_ignore_ascii_case("boolean") {
        Some("BOOLEAN")
    } else if kind.eq_ignore_ascii_case("array") {
        Some("ARRAY")
    } else if kind.eq_ignore_ascii_case("object") {
        Some("OBJECT")
    } else if kind.eq_ignore_ascii_case("null") {
        Some("NULL")
    } else {
        None
    }
}

fn resolve_local_refs(
    map: &Map<String, Value>,
    root: &Value,
    visited: &mut HashSet<String>,
) -> (Map<String, Value>, Vec<String>) {
    let mut current = map.clone();
    let mut added = Vec::new();
    for _ in 0..MAX_REF_DEPTH {
        let Some(pointer) = current
            .get("$ref")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            break;
        };
        current.remove("$ref");
        if !pointer.starts_with("#/")
            || visited.len() >= MAX_REF_DEPTH
            || !visited.insert(pointer.clone())
        {
            break;
        }
        added.push(pointer.clone());
        let Some(target) = root.pointer(&pointer[1..]).and_then(Value::as_object) else {
            break;
        };
        let mut merged = target.clone();
        for (k, v) in current {
            merged.insert(k, v);
        }
        current = merged;
    }
    (current, added)
}

fn is_null_branch(branch: &Map<String, Value>) -> bool {
    branch
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("null"))
        || branch.get("const") == Some(&Value::Null)
}

fn branch_string_values(branch: &Map<String, Value>) -> Option<Vec<String>> {
    if let Some(kind) = branch.get("type").and_then(Value::as_str)
        && !kind.eq_ignore_ascii_case("string")
    {
        return None;
    }
    if let Some(c) = branch.get("const").and_then(Value::as_str) {
        return Some(vec![c.to_owned()]);
    }
    let values = branch.get("enum")?.as_array()?;
    if values.is_empty() || !values.iter().all(Value::is_string) {
        return None;
    }
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

fn branch_genai_type(branch: &Map<String, Value>) -> Option<&'static str> {
    if let Some(kind) = branch.get("type").and_then(Value::as_str) {
        return genai_type(kind);
    }
    if let Some(c) = branch.get("const") {
        return match c {
            Value::String(_) => Some("STRING"),
            Value::Bool(_) => Some("BOOLEAN"),
            Value::Number(n) => Some(if n.is_i64() || n.is_u64() {
                "INTEGER"
            } else {
                "NUMBER"
            }),
            Value::Null => Some("NULL"),
            _ => None,
        };
    }
    if branch.get("properties").is_some_and(Value::is_object) {
        return Some("OBJECT");
    }
    if branch.get("items").is_some_and(Value::is_object) {
        return Some("ARRAY");
    }
    None
}

fn allowed_format(kind: &str, format: &str) -> bool {
    match kind {
        "STRING" => matches!(
            format,
            "enum"
                | "date-time"
                | "date"
                | "time"
                | "duration"
                | "email"
                | "idn-email"
                | "hostname"
                | "idn-hostname"
                | "ipv4"
                | "ipv6"
                | "uuid"
                | "uri"
                | "uri-reference"
                | "regex"
                | "json-pointer"
                | "relative-json-pointer"
        ),
        "INTEGER" => matches!(format, "int32" | "int64"),
        "NUMBER" => matches!(format, "float" | "double"),
        _ => false,
    }
}

fn project_schema(
    schema: &Value,
    root: &Value,
    visited: &mut HashSet<String>,
    depth: usize,
    is_root: bool,
    budget: &mut ProjectionBudget,
) -> Value {
    if depth > MAX_SCHEMA_DEPTH {
        return if is_root {
            json!({"type": "OBJECT"})
        } else {
            json!({})
        };
    }
    let Some(raw_map) = schema.as_object() else {
        return if is_root {
            json!({"type": "OBJECT"})
        } else {
            json!({})
        };
    };
    let (mut map, added_refs) = resolve_local_refs(raw_map, root, visited);

    // Fold simple `allOf` object compositions (common in OpenAPI / MCP).
    if let Some(all_of) = map.get("allOf").and_then(Value::as_array).cloned() {
        for item in &all_of {
            let Some(branch_raw) = item.as_object() else {
                continue;
            };
            let (branch, branch_refs) = resolve_local_refs(branch_raw, root, visited);
            for r in branch_refs {
                visited.remove(&r);
            }
            for key in ["type", "description", "items"] {
                if !map.contains_key(key)
                    && let Some(val) = branch.get(key)
                {
                    map.insert(key.to_owned(), val.clone());
                }
            }
            if let Some(branch_props) = branch.get("properties").and_then(Value::as_object) {
                let target = map
                    .entry("properties")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(target_props) = target.as_object_mut() {
                    for (k, v) in branch_props {
                        target_props.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            if let Some(branch_req) = branch.get("required").and_then(Value::as_array) {
                let target = map
                    .entry("required")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(target_req) = target.as_array_mut() {
                    for req in branch_req {
                        if req.is_string() && !target_req.contains(req) {
                            target_req.push(req.clone());
                        }
                    }
                }
            }
        }
    }

    let mut nullable = map.get("nullable") == Some(&Value::Bool(true));
    let mut inferred_union_type: Option<&'static str> = None;

    let union_branches = map
        .get("anyOf")
        .or_else(|| map.get("oneOf"))
        .and_then(Value::as_array)
        .cloned();
    if let Some(branches) = union_branches {
        let mut null_count = 0usize;
        let mut non_null = Vec::new();
        let mut saw_non_object = false;
        for item in &branches {
            if let Some(branch_raw) = item.as_object() {
                let (resolved, branch_refs) = resolve_local_refs(branch_raw, root, visited);
                for r in branch_refs {
                    visited.remove(&r);
                }
                if is_null_branch(&resolved) {
                    null_count += 1;
                } else {
                    non_null.push(resolved);
                }
            } else {
                saw_non_object = true;
            }
        }
        if null_count > 0 {
            nullable = true;
        }
        if !saw_non_object {
            if non_null.len() == 1 && null_count > 0 {
                // Unwrap nullable `Optional[T]` (`anyOf: [T, {"type": "null"}]`).
                for (k, v) in &non_null[0] {
                    if !map.contains_key(k) {
                        map.insert(k.clone(), v.clone());
                    }
                }
            } else if !non_null.is_empty()
                && non_null.iter().all(|b| branch_string_values(b).is_some())
            {
                // Flatten string enum/const unions (e.g., `TaskUpdate.status`).
                let mut combined = Vec::new();
                for b in &non_null {
                    for s in branch_string_values(b).unwrap() {
                        if !combined.contains(&s) {
                            combined.push(s);
                        }
                    }
                }
                if !map.contains_key("type") {
                    map.insert("type".into(), json!("string"));
                }
                if !map.contains_key("enum") && !combined.is_empty() {
                    map.insert("enum".into(), json!(combined));
                }
            } else if !non_null.is_empty() && !map.contains_key("type") {
                let types: Option<Vec<&'static str>> =
                    non_null.iter().map(branch_genai_type).collect();
                if let Some(types) = types {
                    let first = types[0];
                    if types.iter().all(|t| *t == first) {
                        inferred_union_type = Some(first);
                    } else if types.iter().all(|t| matches!(*t, "INTEGER" | "NUMBER")) {
                        inferred_union_type = Some("NUMBER");
                    }
                }
            }
        }
    }

    let mut kind: Option<&'static str> = match map.get("type") {
        Some(Value::String(s)) => genai_type(s),
        Some(Value::Array(arr)) => {
            let mut non_null_types: Vec<&'static str> = Vec::new();
            let mut saw_null = false;
            let mut valid = !arr.is_empty();
            for entry in arr {
                let Some(s) = entry.as_str().and_then(genai_type) else {
                    valid = false;
                    break;
                };
                if s == "NULL" {
                    saw_null = true;
                } else if !non_null_types.contains(&s) {
                    non_null_types.push(s);
                }
            }
            if !valid {
                None
            } else {
                if saw_null {
                    nullable = true;
                }
                match non_null_types.as_slice() {
                    [single] => Some(*single),
                    [a, b] if matches!((*a, *b), ("INTEGER", "NUMBER") | ("NUMBER", "INTEGER")) => {
                        Some("NUMBER")
                    }
                    [] if saw_null => Some("NULL"),
                    _ => None,
                }
            }
        }
        _ => inferred_union_type,
    };

    if let Some(c) = map.get("const") {
        match c {
            Value::String(s) => {
                if kind.is_none() {
                    kind = Some("STRING");
                }
                if kind == Some("STRING") && !map.contains_key("enum") {
                    map.insert("enum".into(), json!([s]));
                }
            }
            Value::Bool(_) if kind.is_none() => kind = Some("BOOLEAN"),
            Value::Number(n) if kind.is_none() => {
                kind = Some(if n.is_i64() || n.is_u64() {
                    "INTEGER"
                } else {
                    "NUMBER"
                });
            }
            Value::Null if kind.is_none() => kind = Some("NULL"),
            _ => {}
        }
    }

    if kind.is_none() {
        if map.get("properties").is_some_and(Value::is_object) {
            kind = Some("OBJECT");
        } else if map.get("items").is_some_and(Value::is_object) {
            kind = Some("ARRAY");
        }
    }
    if is_root && kind.is_none() {
        kind = Some("OBJECT");
    }

    let mut out = Map::new();
    if let Some(t) = kind {
        out.insert("type".into(), json!(t));
    }
    if nullable && !is_root && !matches!(kind, None | Some("NULL")) {
        out.insert("nullable".into(), json!(true));
    }
    if let Some(desc) = map
        .get("description")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        out.insert("description".into(), json!(desc));
    }

    match kind {
        Some("STRING") => {
            if let Some(fmt) = map
                .get("format")
                .and_then(Value::as_str)
                .filter(|f| allowed_format("STRING", f))
            {
                out.insert("format".into(), json!(fmt));
            }
            if let Some(pat) = map
                .get("pattern")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                out.insert("pattern".into(), json!(pat));
            }
            if let Some(values) = map.get("enum").and_then(Value::as_array) {
                let strings: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
                if !strings.is_empty() {
                    out.insert("enum".into(), json!(strings));
                }
            }
            for key in ["minLength", "maxLength"] {
                if let Some(value) = map.get(key).and_then(length_value).filter(|v| *v != 0) {
                    out.insert(key.into(), json!(value.to_string()));
                }
            }
        }
        Some("INTEGER" | "NUMBER") => {
            let t = kind.unwrap();
            if let Some(fmt) = map
                .get("format")
                .and_then(Value::as_str)
                .filter(|f| allowed_format(t, f))
            {
                out.insert("format".into(), json!(fmt));
            }
            for key in ["minimum", "maximum"] {
                if let Some(value) = map.get(key).and_then(number_value).filter(|v| *v != 0.0) {
                    out.insert(key.into(), json!(value));
                }
            }
        }
        Some("OBJECT") => {
            if let Some(properties) = map.get("properties").and_then(Value::as_object)
                && !properties.is_empty()
            {
                let projected_props: Map<String, Value> = properties
                    .iter()
                    .map(|(name, child)| {
                        (
                            name.clone(),
                            project_schema(child, root, visited, depth + 1, false, budget),
                        )
                    })
                    .collect();
                out.insert("properties".into(), Value::Object(projected_props));
                if let Some(required) = map.get("required").and_then(Value::as_array) {
                    let mut req_names: Vec<&str> = Vec::new();
                    for entry in required {
                        if let Some(name) = entry.as_str()
                            && properties.contains_key(name)
                            && !req_names.contains(&name)
                        {
                            req_names.push(name);
                        }
                    }
                    if !req_names.is_empty() {
                        out.insert("required".into(), json!(req_names));
                    }
                }
            }
        }
        Some("ARRAY") => {
            if let Some(items) = map.get("items")
                && items.is_object()
            {
                out.insert(
                    "items".into(),
                    project_schema(items, root, visited, depth + 1, false, budget),
                );
            }
            for key in ["minItems", "maxItems"] {
                if let Some(value) = map.get(key).and_then(length_value).filter(|v| *v != 0) {
                    out.insert(key.into(), json!(value.to_string()));
                }
            }
        }
        _ => {}
    }

    if let Some(default_val) = map.get("default") {
        let mut candidate = budget.clone();
        if candidate.visit(default_val, 0) {
            *budget = candidate;
            out.insert("default".into(), default_val.clone());
        }
    }

    for r in added_refs {
        visited.remove(&r);
    }

    Value::Object(out)
}
