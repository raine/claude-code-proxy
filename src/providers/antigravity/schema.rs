//! Source validation and honest Gemini generation projection are separate layers.
use super::replay::{self, Result, keys, nonempty};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

const MAX_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_SCHEMA_NODES: usize = 8192;

struct Offline;
impl jsonschema::Retrieve for Offline {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema retrieval disabled".into())
    }
}

enum Codec {
    Identity,
    Json,
    Object(BTreeMap<String, Codec>),
    Array(Box<Codec>),
}

struct Tool {
    caller: String,
    native: String,
    validator: jsonschema::Validator,
    codec: Codec,
    root: bool,
    empty: bool,
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
                &["name", "description", "input_schema", "cache_control"],
                &["name", "input_schema"],
            )?;
            super::request::cache_hint(tool.get("cache_control"))?;
            let caller = nonempty(&tool["name"])?;
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
            let mut nodes = 0;
            validate_source(schema, 0, &mut nodes)?;
            let draft = match schema.get("$schema").and_then(Value::as_str) {
                None | Some("https://json-schema.org/draft/2020-12/schema") => {
                    jsonschema::Draft::Draft202012
                }
                Some(
                    "http://json-schema.org/draft-07/schema"
                    | "http://json-schema.org/draft-07/schema#",
                ) => jsonschema::Draft::Draft7,
                _ => return Err("unsupported schema dialect"),
            };
            let validator = jsonschema::options()
                .with_draft(draft)
                .should_validate_formats(true)
                .with_retriever(Offline)
                .build(schema)
                .map_err(|_| "invalid original tool schema")?;
            let (projected, codec) = project(schema)?;
            let empty = schema["type"] == "object"
                && schema["additionalProperties"] == false
                && schema
                    .get("properties")
                    .is_none_or(|v| v.as_object().is_some_and(|v| v.is_empty()));
            let root = matches!(codec, Codec::Json);
            let mut declaration = json!({"name":native});
            if let Some(description) = tool.get("description") {
                let text = description
                    .as_str()
                    .filter(|v| v.len() <= 32768)
                    .ok_or("invalid tool description")?;
                declaration["description"] = json!(text);
            }
            if !empty {
                declaration["parameters"] = if root {
                    json!({"type":"OBJECT","properties":{"json":projected},"required":["json"],"description":"Encode the entire original caller argument object as JSON in the single json string field; no extra wrapper keys."})
                } else {
                    projected
                };
            }
            declarations.push(declaration);
            compiled.push(Tool {
                caller: caller.to_string(),
                native,
                validator,
                codec,
                root,
                empty,
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
            let mut paths = Vec::new();
            let root = if tool.root {
                json!("json")
            } else {
                Value::Null
            };
            let raw_args = replay::call_arguments(call)?;
            if !tool.root {
                codec_paths(&tool.codec, raw_args, "", &mut paths)?;
            }
            let args = replay::decode_arguments(raw_args, &json!(paths), &root)?;
            if (tool.empty && args != json!({})) || !tool.validator.is_valid(&args) {
                return Err("upstream tool arguments violate original schema");
            }
            recipes
                .push(json!({"caller_name":tool.caller,"codec_paths":paths,"root_wrapper":root}));
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
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
}

fn required_undeclared(schema: &Value) -> bool {
    let properties = schema.get("properties").and_then(Value::as_object);
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .any(|name| properties.is_none_or(|props| !props.contains_key(name)))
        })
}

fn validate_source(schema: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    if depth > 32 || *nodes > MAX_SCHEMA_NODES {
        return Err("schema depth/node limit");
    }
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema
        .as_object()
        .ok_or("schema must be object or boolean")?;
    if schema["type"] == "object"
        && schema["additionalProperties"] == false
        && required_undeclared(schema)
    {
        return Err(
            "unsatisfiable tool contract: required undeclared property is forbidden by additionalProperties:false",
        );
    }
    let allowed = [
        "$schema",
        "type",
        "properties",
        "required",
        "items",
        "additionalProperties",
        "allOf",
        "anyOf",
        "oneOf",
        "default",
        "description",
        "title",
        "enum",
        "const",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "multipleOf",
        "uniqueItems",
        "minProperties",
        "maxProperties",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unsupported schema keyword or reference");
    }
    if let Some(dialect) = object.get("$schema") {
        if !matches!(
            dialect.as_str(),
            Some(
                "https://json-schema.org/draft/2020-12/schema"
                    | "http://json-schema.org/draft-07/schema"
                    | "http://json-schema.org/draft-07/schema#"
            )
        ) {
            return Err("unsupported schema dialect");
        }
        if depth != 0 {
            return Err("nested dialect is unsupported");
        }
    }
    if let Some(format) = object.get("format")
        && !matches!(
            format.as_str(),
            Some(
                "date-time"
                    | "date"
                    | "time"
                    | "duration"
                    | "email"
                    | "idn-email"
                    | "hostname"
                    | "idn-hostname"
                    | "ipv4"
                    | "ipv6"
                    | "uri"
                    | "uri-reference"
                    | "uuid"
                    | "regex"
                    | "json-pointer"
                    | "relative-json-pointer"
            )
        )
    {
        return Err("unsupported schema format");
    }
    if let Some(properties) = object.get("properties") {
        for child in properties
            .as_object()
            .ok_or("invalid schema properties")?
            .values()
        {
            validate_source(child, depth + 1, nodes)?;
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(child) = object.get(key) {
            validate_source(child, depth + 1, nodes)?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = object.get(key) {
            for child in branches.as_array().ok_or("invalid schema branches")? {
                validate_source(child, depth + 1, nodes)?;
            }
        }
    }
    Ok(())
}

fn json_codec(schema: &Value) -> Result<(Value, Codec)> {
    // Complete deterministic JSON, not a lossy summary: the model must know the
    // caller's names/types/requirements even when the native carrier is STRING.
    let original = serde_json::to_string(schema).map_err(|_| "invalid original schema guidance")?;
    let description = format!(
        "JSON-string transport: encode the original JSON value exactly as a JSON string. The proxy decodes and validates this original schema before returning a tool call. Original JSON Schema: {original}"
    );
    if description.len() > 16384 {
        return Err("complete schema guidance exceeds the 16 KiB limit");
    }
    Ok((
        json!({"type":"STRING","description":description}),
        Codec::Json,
    ))
}

fn project(schema: &Value) -> Result<(Value, Codec)> {
    let Some(kind) = schema.get("type").and_then(Value::as_str) else {
        // Only a bare union (apart from dialect metadata) can return directly.
        // Otherwise sibling constraints/annotations would bypass the normal
        // projection path. Use the complete bounded JSON codec, not just anyOf.
        if let Some(branches) = schema.get("anyOf").and_then(Value::as_array)
            && schema.as_object().is_some_and(|fields| {
                fields
                    .keys()
                    .all(|key| matches!(key.as_str(), "anyOf" | "$schema"))
            })
        {
            let projected = branches.iter().map(project).collect::<Result<Vec<_>>>()?;
            if projected.iter().all(|(_, c)| identity(c)) {
                return Ok((
                    json!({"anyOf":projected.into_iter().map(|(s,_)|s).collect::<Vec<_>>()}),
                    Codec::Identity,
                ));
            }
        }
        return json_codec(schema);
    };
    if schema.get("oneOf").is_some()
        || !matches!(
            kind,
            "object" | "array" | "string" | "integer" | "number" | "boolean"
        )
    {
        return json_codec(schema);
    }
    let mut out = Map::new();
    out.insert("type".into(), json!(kind.to_ascii_uppercase()));
    let mut codec = Codec::Identity;
    if kind == "object" {
        let props = schema.get("properties").and_then(Value::as_object);
        if required_undeclared(schema) {
            if schema["additionalProperties"] == false {
                return Err(
                    "unsatisfiable tool contract: required undeclared property is forbidden by additionalProperties:false",
                );
            }
            return json_codec(schema);
        }
        if props.is_none_or(|p| p.is_empty()) && schema["additionalProperties"] != false {
            return json_codec(schema);
        }
        if schema
            .get("additionalProperties")
            .is_some_and(Value::is_object)
        {
            return json_codec(schema);
        }
        let mut projected = Map::new();
        let mut children = BTreeMap::new();
        for (name, child) in props.into_iter().flatten() {
            let (schema, child_codec) = project(child)?;
            projected.insert(name.clone(), schema);
            children.insert(name.clone(), child_codec);
        }
        out.insert("properties".into(), Value::Object(projected));
        codec = Codec::Object(children);
        if let Some(required) = schema.get("required") {
            out.insert("required".into(), required.clone());
        }
    } else if kind == "array" {
        let (item, item_codec) = project(schema.get("items").unwrap_or(&Value::Bool(true)))?;
        out.insert("items".into(), item);
        codec = Codec::Array(Box::new(item_codec));
    }
    for key in [
        "description",
        "title",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "pattern",
    ] {
        if let Some(value) = schema.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if let Some(values) = schema
        .get("enum")
        .and_then(Value::as_array)
        .filter(|v| v.iter().all(Value::is_string))
    {
        out.insert("enum".into(), json!(values));
    }
    for (exclusive, inclusive, lower) in [
        ("exclusiveMinimum", "minimum", true),
        ("exclusiveMaximum", "maximum", false),
    ] {
        if kind == "integer"
            && let Some(value) = schema.get(exclusive).and_then(Value::as_f64)
        {
            let bound = if lower {
                value.floor() + 1.0
            } else {
                value.ceil() - 1.0
            };
            if bound.abs() <= 9007199254740991.0 {
                let existing = out.get(inclusive).and_then(Value::as_f64);
                let bound = existing.map_or(bound, |old| {
                    if lower {
                        old.ceil().max(bound)
                    } else {
                        old.floor().min(bound)
                    }
                });
                if bound.abs() <= 9007199254740991.0 {
                    out.insert(inclusive.to_string(), json!(bound as i64));
                }
            }
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let projected = branches.iter().map(project).collect::<Result<Vec<_>>>()?;
        if !projected.iter().all(|(_, c)| identity(c)) {
            return json_codec(schema);
        }
        out.insert(
            "anyOf".into(),
            json!(projected.into_iter().map(|(v, _)| v).collect::<Vec<_>>()),
        );
    }
    let local = [
        "additionalProperties",
        "allOf",
        "format",
        "multipleOf",
        "uniqueItems",
        "minProperties",
        "maxProperties",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "const",
    ];
    let mut constraints: Map<String, Value> = local
        .iter()
        .filter_map(|k| schema.get(*k).map(|v| ((*k).to_string(), v.clone())))
        .collect();
    if let Some(values) = schema
        .get("enum")
        .and_then(Value::as_array)
        .filter(|values| values.iter().any(|v| !v.is_string()))
    {
        constraints.insert("enum".to_string(), json!(values));
    }
    if !constraints.is_empty() {
        let hint = format!(
            "Original schema constraints are validated locally after JSON decoding; invalid calls are rejected, never repaired. Constraints: {}",
            Value::Object(constraints)
        );
        let prior = out.get("description").and_then(Value::as_str).unwrap_or("");
        let description = format!("{prior}\n{hint}");
        if description.len() > 16384 {
            return Err("complete schema guidance exceeds the 16 KiB limit");
        }
        out.insert("description".into(), json!(description));
    }
    Ok((Value::Object(out), codec))
}

fn identity(codec: &Codec) -> bool {
    match codec {
        Codec::Identity => true,
        Codec::Json => false,
        Codec::Object(c) => c.values().all(identity),
        Codec::Array(c) => identity(c),
    }
}

fn codec_paths(codec: &Codec, args: &Value, path: &str, paths: &mut Vec<String>) -> Result<()> {
    match codec {
        Codec::Json => paths.push(path.to_string()),
        Codec::Object(children) => {
            if let Some(object) = args.as_object() {
                for (name, child) in children {
                    if let Some(value) = object.get(name) {
                        codec_paths(
                            child,
                            value,
                            &format!("{path}/{}", name.replace('~', "~0").replace('/', "~1")),
                            paths,
                        )?;
                    }
                }
            }
        }
        Codec::Array(child) => {
            if let Some(array) = args.as_array() {
                for (i, value) in array.iter().enumerate() {
                    codec_paths(child, value, &format!("{path}/{i}"), paths)?;
                }
            }
        }
        Codec::Identity => {}
    }
    if paths.len() > 64 {
        return Err("codec path count limit");
    }
    Ok(())
}
