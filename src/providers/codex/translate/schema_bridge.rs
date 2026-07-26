use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;

use crate::providers::translate_shared::normalize_strict_json_schema;

/// A request-local bridge between the schema promised to the Anthropic caller
/// and the stricter schema sent to the Codex Responses endpoint.
///
/// Validators are compiled once when the request is translated and can be
/// shared by buffered and live response paths. The `jsonschema` dependency is
/// built without its HTTP and file resolvers; preflight validation additionally
/// rejects every non-local `$ref` before either validator is constructed.
pub(crate) struct SchemaBridge {
    original: Arc<Value>,
    upstream: Arc<Value>,
    original_validator: Arc<jsonschema::Validator>,
    upstream_validator: Arc<jsonschema::Validator>,
    null_elision: Arc<NullElisionPlan>,
}

impl std::fmt::Debug for SchemaBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SchemaBridge")
            .field("original", &self.original)
            .field("upstream", &self.upstream)
            .finish_non_exhaustive()
    }
}

impl SchemaBridge {
    pub(crate) fn build(original: &Value) -> Result<Self, SchemaBridgeBuildError> {
        preflight_schema(original)?;

        let original_validator = compile_validator(original, SchemaSide::Original)?;
        let upstream = normalize_strict_json_schema(original);
        let upstream_validator = compile_validator(&upstream, SchemaSide::Upstream)?;
        let null_elision = NullElisionPlan::build(original)?;

        Ok(Self {
            original: Arc::new(original.clone()),
            upstream: Arc::new(upstream),
            original_validator: Arc::new(original_validator),
            upstream_validator: Arc::new(upstream_validator),
            null_elision: Arc::new(null_elision),
        })
    }

    #[cfg(test)]
    pub(crate) fn original_schema(&self) -> &Value {
        self.original.as_ref()
    }

    pub(crate) fn upstream_schema(&self) -> &Value {
        self.upstream.as_ref()
    }

    /// Validate a completed structured-output text and reverse only the
    /// optional-null widening introduced by this bridge.
    ///
    /// A value already valid under the caller's schema is returned byte-for-byte.
    /// Otherwise it must first be valid under the exact upstream schema. The
    /// decoder then removes only null-valued properties recorded as synthetic,
    /// and the entire result is validated against the original schema again.
    pub(crate) fn normalize_completed_text<'a>(
        &self,
        raw: &'a str,
    ) -> Result<NormalizedStructuredText<'a>, StructuredOutputError> {
        let value: Value =
            serde_json::from_str(raw).map_err(|error| StructuredOutputError::InvalidJson {
                line: error.line(),
                column: error.column(),
            })?;

        if self.original_validator.is_valid(&value) {
            return Ok(NormalizedStructuredText {
                text: Cow::Borrowed(raw),
            });
        }

        if !self.upstream_validator.is_valid(&value) {
            return Err(validation_error(
                &self.upstream_validator,
                &value,
                ValidationSide::Upstream,
            ));
        }

        let mut normalized = value;
        self.null_elision.apply(&mut normalized);
        if !self.original_validator.is_valid(&normalized) {
            return Err(validation_error(
                &self.original_validator,
                &normalized,
                ValidationSide::Original,
            ));
        }

        let text = serde_json::to_string(&normalized)
            .map_err(|error| StructuredOutputError::Serialization(error.to_string()))?;
        Ok(NormalizedStructuredText {
            text: Cow::Owned(text),
        })
    }
}

#[derive(Debug)]
pub(crate) struct NormalizedStructuredText<'a> {
    pub(crate) text: Cow<'a, str>,
}

#[derive(Debug, Error)]
pub(crate) enum SchemaBridgeBuildError {
    #[error("unsupported structured-output schema at {path}: {reason}")]
    Unsupported { path: String, reason: String },
    #[error(
        "invalid original structured-output schema at instance path {instance_path} (schema path {schema_path})"
    )]
    InvalidOriginal {
        instance_path: String,
        schema_path: String,
    },
    #[error(
        "invalid projected upstream schema at instance path {instance_path} (schema path {schema_path})"
    )]
    InvalidUpstream {
        instance_path: String,
        schema_path: String,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum StructuredOutputError {
    #[error("completed structured output is not valid JSON at line {line}, column {column}")]
    InvalidJson { line: usize, column: usize },
    #[error(
        "completed structured output violates the projected upstream schema at instance path {instance_path} (schema path {schema_path})"
    )]
    UpstreamViolation {
        instance_path: String,
        schema_path: String,
    },
    #[error(
        "completed structured output violates the original schema at instance path {instance_path} (schema path {schema_path})"
    )]
    OriginalViolation {
        instance_path: String,
        schema_path: String,
    },
    #[error("completed structured output could not be serialized: {0}")]
    Serialization(String),
}

#[derive(Clone, Copy)]
enum SchemaSide {
    Original,
    Upstream,
}

fn compile_validator(
    schema: &Value,
    side: SchemaSide,
) -> Result<jsonschema::Validator, SchemaBridgeBuildError> {
    jsonschema::draft202012::options()
        .build(schema)
        .map_err(|error| match side {
            SchemaSide::Original => SchemaBridgeBuildError::InvalidOriginal {
                instance_path: error.instance_path().to_string(),
                schema_path: error.schema_path().to_string(),
            },
            SchemaSide::Upstream => SchemaBridgeBuildError::InvalidUpstream {
                instance_path: error.instance_path().to_string(),
                schema_path: error.schema_path().to_string(),
            },
        })
}

#[derive(Clone, Copy)]
enum ValidationSide {
    Original,
    Upstream,
}

fn validation_error(
    validator: &jsonschema::Validator,
    instance: &Value,
    side: ValidationSide,
) -> StructuredOutputError {
    let Some(error) = validator.iter_errors(instance).next() else {
        return StructuredOutputError::Serialization(
            "validator reported an inconsistent result".to_string(),
        );
    };
    let instance_path = error.instance_path().to_string();
    let schema_path = error.schema_path().to_string();
    match side {
        ValidationSide::Original => StructuredOutputError::OriginalViolation {
            instance_path,
            schema_path,
        },
        ValidationSide::Upstream => StructuredOutputError::UpstreamViolation {
            instance_path,
            schema_path,
        },
    }
}

#[derive(Default)]
struct NullElisionPlan {
    object: Option<ObjectPlan>,
    array: Option<ArrayPlan>,
}

impl NullElisionPlan {
    fn build(root: &Value) -> Result<Self, SchemaBridgeBuildError> {
        let mut active_refs = Vec::new();
        build_plan(root, root, "#", &mut active_refs)
    }

    fn apply(&self, instance: &mut Value) {
        match instance {
            Value::Object(object) => {
                if let Some(plan) = &self.object {
                    plan.apply(object);
                }
            }
            Value::Array(array) => {
                if let Some(plan) = &self.array {
                    plan.apply(array);
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct ObjectPlan {
    properties: BTreeMap<String, PropertyPlan>,
}

impl ObjectPlan {
    fn apply(&self, instance: &mut serde_json::Map<String, Value>) {
        for (name, property) in &self.properties {
            if property.elide_when_null && instance.get(name).is_some_and(Value::is_null) {
                instance.remove(name);
                continue;
            }
            if let Some(value) = instance.get_mut(name) {
                property.nested.apply(value);
            }
        }
    }
}

struct PropertyPlan {
    elide_when_null: bool,
    nested: NullElisionPlan,
}

#[derive(Default)]
struct ArrayPlan {
    prefix_items: Vec<NullElisionPlan>,
    items: Option<Box<NullElisionPlan>>,
}

impl ArrayPlan {
    fn apply(&self, instance: &mut [Value]) {
        for (index, value) in instance.iter_mut().enumerate() {
            if let Some(plan) = self.prefix_items.get(index) {
                plan.apply(value);
            } else if let Some(plan) = self.items.as_deref() {
                plan.apply(value);
            }
        }
    }
}

fn build_plan(
    schema: &Value,
    root: &Value,
    path: &str,
    active_refs: &mut Vec<String>,
) -> Result<NullElisionPlan, SchemaBridgeBuildError> {
    let Some(object) = schema.as_object() else {
        return Ok(NullElisionPlan::default());
    };

    if let Some(reference) = object.get("$ref") {
        let reference = reference.as_str().ok_or_else(|| {
            unsupported(
                path,
                "$ref must be a string containing a local JSON Pointer fragment",
            )
        })?;
        return with_resolved_ref(
            reference,
            root,
            path,
            active_refs,
            |target, target_path, refs| build_plan(target, root, target_path, refs),
        );
    }

    let mut plan = NullElisionPlan::default();
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        let required: HashSet<&str> = object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let mut property_plans = BTreeMap::new();
        for (name, property_schema) in properties {
            let property_path = join_pointer(&join_pointer(path, "properties"), name);
            let accepts_null =
                schema_accepts_null(property_schema, root, &property_path, &mut Vec::new())?;
            property_plans.insert(
                name.clone(),
                PropertyPlan {
                    elide_when_null: !required.contains(name.as_str()) && !accepts_null,
                    nested: build_plan(property_schema, root, &property_path, active_refs)?,
                },
            );
        }
        plan.object = Some(ObjectPlan {
            properties: property_plans,
        });
    }

    let prefix_items = object
        .get("prefixItems")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    build_plan(
                        item,
                        root,
                        &join_pointer(&join_pointer(path, "prefixItems"), &index.to_string()),
                        active_refs,
                    )
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let items = match object.get("items") {
        Some(items) if items.is_object() => Some(Box::new(build_plan(
            items,
            root,
            &join_pointer(path, "items"),
            active_refs,
        )?)),
        _ => None,
    };
    if !prefix_items.is_empty() || items.is_some() {
        plan.array = Some(ArrayPlan {
            prefix_items,
            items,
        });
    }
    Ok(plan)
}

fn schema_accepts_null(
    schema: &Value,
    root: &Value,
    path: &str,
    active_refs: &mut Vec<String>,
) -> Result<bool, SchemaBridgeBuildError> {
    match schema {
        Value::Bool(value) => Ok(*value),
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref") {
                let reference = reference.as_str().ok_or_else(|| {
                    unsupported(
                        path,
                        "$ref must be a string containing a local JSON Pointer fragment",
                    )
                })?;
                return with_resolved_ref(
                    reference,
                    root,
                    path,
                    active_refs,
                    |target, target_path, refs| {
                        schema_accepts_null(target, root, target_path, refs)
                    },
                );
            }

            let type_allows = match object.get("type") {
                None => true,
                Some(Value::String(kind)) => kind == "null",
                Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("null")),
                Some(_) => false,
            };
            let const_allows = object.get("const").is_none_or(Value::is_null);
            let enum_allows = object
                .get("enum")
                .and_then(Value::as_array)
                .is_none_or(|values| values.iter().any(Value::is_null));
            Ok(type_allows && const_allows && enum_allows)
        }
        _ => Ok(false),
    }
}

fn preflight_schema(root: &Value) -> Result<(), SchemaBridgeBuildError> {
    let mut active_refs = Vec::new();
    let mut completed_refs = HashSet::new();
    preflight_node(root, root, "#", &mut active_refs, &mut completed_refs)
}

fn preflight_node(
    schema: &Value,
    root: &Value,
    path: &str,
    active_refs: &mut Vec<String>,
    completed_refs: &mut HashSet<String>,
) -> Result<(), SchemaBridgeBuildError> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };

    if path != "#" {
        for keyword in ["$id", "$anchor", "$dynamicAnchor"] {
            if object.contains_key(keyword) {
                return Err(unsupported(
                    &join_pointer(path, keyword),
                    "nested schema resource identifiers are not supported; declare identifiers only at the root schema",
                ));
            }
        }
    }

    for keyword in [
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "dependentSchemas",
        "patternProperties",
        "contains",
        "unevaluatedItems",
        "unevaluatedProperties",
        "$dynamicRef",
        "$recursiveRef",
    ] {
        if object.contains_key(keyword) {
            return Err(unsupported(
                &join_pointer(path, keyword),
                &format!(
                    "{keyword} makes optional-null reversal ambiguous in the first SchemaBridge implementation"
                ),
            ));
        }
    }

    if object
        .get("additionalProperties")
        .is_some_and(|value| value.is_object())
    {
        return Err(unsupported(
            &join_pointer(path, "additionalProperties"),
            "schema-valued additionalProperties cannot be mapped to deterministic instance paths",
        ));
    }
    if object
        .get("additionalItems")
        .is_some_and(|value| value.is_object())
    {
        return Err(unsupported(
            &join_pointer(path, "additionalItems"),
            "schema-valued additionalItems is not part of the supported Draft 2020-12 bridge subset",
        ));
    }
    if object
        .get("dependencies")
        .and_then(Value::as_object)
        .is_some_and(|dependencies| {
            dependencies
                .values()
                .any(|value| value.is_object() || value.is_boolean())
        })
    {
        return Err(unsupported(
            &join_pointer(path, "dependencies"),
            "schema dependencies make optional-null reversal ambiguous",
        ));
    }

    if let Some(reference) = object.get("$ref") {
        let reference = reference.as_str().ok_or_else(|| {
            unsupported(
                &join_pointer(path, "$ref"),
                "$ref must be a string containing a local JSON Pointer fragment",
            )
        })?;
        reject_structural_ref_siblings(object, path)?;
        preflight_resolved_ref(reference, root, path, active_refs, completed_refs)?;
    }

    for keyword in ["properties", "$defs", "definitions"] {
        if let Some(schemas) = object.get(keyword).and_then(Value::as_object) {
            for (name, nested) in schemas {
                preflight_node(
                    nested,
                    root,
                    &join_pointer(&join_pointer(path, keyword), name),
                    active_refs,
                    completed_refs,
                )?;
            }
        }
    }
    if let Some(items) = object.get("items")
        && (items.is_object() || items.is_boolean())
    {
        preflight_node(
            items,
            root,
            &join_pointer(path, "items"),
            active_refs,
            completed_refs,
        )?;
    }
    if let Some(items) = object.get("prefixItems").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            preflight_node(
                item,
                root,
                &join_pointer(&join_pointer(path, "prefixItems"), &index.to_string()),
                active_refs,
                completed_refs,
            )?;
        }
    }
    for keyword in ["propertyNames", "contentSchema"] {
        if let Some(nested) = object.get(keyword)
            && (nested.is_object() || nested.is_boolean())
        {
            preflight_node(
                nested,
                root,
                &join_pointer(path, keyword),
                active_refs,
                completed_refs,
            )?;
        }
    }
    Ok(())
}

fn preflight_resolved_ref(
    reference: &str,
    root: &Value,
    source_path: &str,
    active_refs: &mut Vec<String>,
    completed_refs: &mut HashSet<String>,
) -> Result<(), SchemaBridgeBuildError> {
    let (target, target_path) = resolve_local_ref(reference, root, source_path)?;
    if completed_refs.contains(reference) {
        return Ok(());
    }
    if active_refs.iter().any(|active| active == reference) {
        return Err(unsupported(
            source_path,
            &format!("recursive local $ref {reference:?} is not supported"),
        ));
    }
    active_refs.push(reference.to_string());
    let result = preflight_node(target, root, &target_path, active_refs, completed_refs);
    active_refs.pop();
    result?;
    completed_refs.insert(reference.to_string());
    Ok(())
}

fn reject_structural_ref_siblings(
    object: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), SchemaBridgeBuildError> {
    const ANNOTATION_KEYS: &[&str] = &[
        "$ref",
        "$id",
        "$schema",
        "$comment",
        "$defs",
        "definitions",
        "title",
        "description",
        "default",
        "deprecated",
        "readOnly",
        "writeOnly",
        "examples",
    ];
    if let Some(key) = object
        .keys()
        .find(|key| !ANNOTATION_KEYS.contains(&key.as_str()))
    {
        return Err(unsupported(
            &join_pointer(path, key),
            "$ref with structural sibling constraints is ambiguous for the first SchemaBridge implementation",
        ));
    }
    Ok(())
}

fn with_resolved_ref<T>(
    reference: &str,
    root: &Value,
    source_path: &str,
    active_refs: &mut Vec<String>,
    f: impl FnOnce(&Value, &str, &mut Vec<String>) -> Result<T, SchemaBridgeBuildError>,
) -> Result<T, SchemaBridgeBuildError> {
    let (target, target_path) = resolve_local_ref(reference, root, source_path)?;
    if active_refs.iter().any(|active| active == reference) {
        return Err(unsupported(
            source_path,
            &format!("recursive local $ref {reference:?} is not supported"),
        ));
    }
    active_refs.push(reference.to_string());
    let result = f(target, &target_path, active_refs);
    active_refs.pop();
    result
}

fn resolve_local_ref<'a>(
    reference: &str,
    root: &'a Value,
    source_path: &str,
) -> Result<(&'a Value, String), SchemaBridgeBuildError> {
    if !reference.starts_with('#') {
        return Err(unsupported(
            source_path,
            &format!("external or relative $ref {reference:?} is not allowed"),
        ));
    }
    if reference.contains('%') {
        return Err(unsupported(
            source_path,
            "percent-encoded local $ref fragments are not supported; use a plain JSON Pointer fragment",
        ));
    }
    let pointer = reference.strip_prefix('#').unwrap_or_default();
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Err(unsupported(
            source_path,
            &format!(
                "named-anchor $ref {reference:?} is not supported; use #/ JSON Pointer syntax"
            ),
        ));
    }
    let target = root.pointer(pointer).ok_or_else(|| {
        unsupported(
            source_path,
            &format!("local $ref {reference:?} does not resolve within the schema"),
        )
    })?;
    if !target.is_object() && !target.is_boolean() {
        return Err(unsupported(
            source_path,
            &format!("local $ref {reference:?} does not resolve to a schema"),
        ));
    }
    Ok((target, reference.to_string()))
}

fn unsupported(path: &str, reason: &str) -> SchemaBridgeBuildError {
    SchemaBridgeBuildError::Unsupported {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

fn join_pointer(path: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    if path == "#" {
        format!("#/{escaped}")
    } else {
        format!("{path}/{escaped}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bridge(schema: Value) -> SchemaBridge {
        SchemaBridge::build(&schema).expect("schema bridge should build")
    }

    #[test]
    fn preserves_original_and_builds_required_nullable_upstream_schema() {
        let original = json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"},
                "reason": {"type": "string"}
            },
            "required": ["ok"],
            "additionalProperties": false
        });
        let bridge = bridge(original.clone());

        assert_eq!(bridge.original_schema(), &original);
        assert_eq!(
            bridge.upstream_schema()["required"],
            json!(["ok", "reason"])
        );
        assert_eq!(
            bridge.upstream_schema()["properties"]["reason"],
            json!({"anyOf":[{"type":"string"},{"type":"null"}]})
        );
    }

    #[test]
    fn returns_original_valid_json_byte_for_byte() {
        let bridge = bridge(json!({
            "type":"object",
            "properties":{"ok":{"type":"boolean"}},
            "required":["ok"],
            "additionalProperties":false
        }));
        let raw = "{ \"ok\" : true }\n";

        let normalized = bridge.normalize_completed_text(raw).unwrap();
        assert!(matches!(normalized.text, Cow::Borrowed(_)));
        assert_eq!(normalized.text, raw);
    }

    #[test]
    fn removes_only_bridge_synthetic_optional_nulls() {
        let bridge = bridge(json!({
            "type":"object",
            "properties":{
                "ok":{"type":"boolean"},
                "reason":{"type":"string"}
            },
            "required":["ok"],
            "additionalProperties":false
        }));

        let normalized = bridge
            .normalize_completed_text(r#"{"ok":true,"reason":null}"#)
            .unwrap();
        assert_eq!(normalized.text, r#"{"ok":true}"#);
    }

    #[test]
    fn projects_impossible_optional_string_const_null_and_elides_synthetic_null() {
        let property_schema = json!({"type":"string","const":null});
        let bridge = bridge(json!({
            "type":"object",
            "properties":{"value":property_schema.clone()},
            "additionalProperties":false
        }));

        assert_eq!(
            bridge.upstream_schema()["properties"]["value"],
            json!({"anyOf":[property_schema,{"type":"null"}]})
        );
        assert_eq!(bridge.upstream_schema()["required"], json!(["value"]));

        let normalized = bridge
            .normalize_completed_text(r#"{"value":null}"#)
            .unwrap();
        assert_eq!(normalized.text, "{}");
    }

    #[test]
    fn projects_optional_nullable_type_with_non_null_enum_and_elides_synthetic_null() {
        let property_schema = json!({"type":["string","null"],"enum":["a"]});
        let bridge = bridge(json!({
            "type":"object",
            "properties":{"value":property_schema.clone()},
            "additionalProperties":false
        }));

        assert_eq!(
            bridge.upstream_schema()["properties"]["value"],
            json!({"anyOf":[property_schema,{"type":"null"}]})
        );
        assert_eq!(bridge.upstream_schema()["required"], json!(["value"]));

        let normalized = bridge
            .normalize_completed_text(r#"{"value":null}"#)
            .unwrap();
        assert_eq!(normalized.text, "{}");
    }

    #[test]
    fn preserves_originally_nullable_optional_and_required_nulls() {
        let optional = bridge(json!({
            "type":"object",
            "properties":{"value":{"type":["string","null"]}},
            "additionalProperties":false
        }));
        let required = bridge(json!({
            "type":"object",
            "properties":{"value":{"type":["string","null"]}},
            "required":["value"],
            "additionalProperties":false
        }));

        for bridge in [&optional, &required] {
            let normalized = bridge
                .normalize_completed_text(r#"{"value":null}"#)
                .unwrap();
            assert_eq!(normalized.text, r#"{"value":null}"#);
        }
    }

    #[test]
    fn preserves_unconstrained_optional_null_while_eliding_synthetic_sibling() {
        let bridge = bridge(json!({
            "type":"object",
            "properties":{
                "free":{},
                "removed":{"type":"string"}
            },
            "additionalProperties":false
        }));

        let normalized = bridge
            .normalize_completed_text(r#"{"free":null,"removed":null}"#)
            .unwrap();
        assert_eq!(normalized.text, r#"{"free":null}"#);
    }

    #[test]
    fn recursively_elides_nested_object_nulls() {
        let bridge = bridge(json!({
            "type":"object",
            "properties":{
                "metadata":{
                    "type":"object",
                    "properties":{"short":{"type":"boolean"}},
                    "required":[],
                    "additionalProperties":false
                }
            },
            "required":["metadata"],
            "additionalProperties":false
        }));

        let normalized = bridge
            .normalize_completed_text(r#"{"metadata":{"short":null}}"#)
            .unwrap();
        assert_eq!(normalized.text, r#"{"metadata":{}}"#);
    }

    #[test]
    fn recursively_elides_array_items_and_prefix_items() {
        let bridge = bridge(json!({
            "type":"array",
            "prefixItems":[{
                "type":"object",
                "properties":{"prefix":{"type":"string"}},
                "additionalProperties":false
            }],
            "items":{
                "type":"object",
                "properties":{"rest":{"type":"integer"}},
                "additionalProperties":false
            }
        }));

        let normalized = bridge
            .normalize_completed_text(r#"[{"prefix":null},{"rest":null}]"#)
            .unwrap();
        assert_eq!(normalized.text, "[{},{}]");
    }

    #[test]
    fn supports_non_recursive_internal_json_pointer_refs() {
        let bridge = bridge(json!({
            "$defs":{
                "entry":{
                    "type":"object",
                    "properties":{"note":{"type":"string"}},
                    "additionalProperties":false
                }
            },
            "$ref":"#/$defs/entry"
        }));

        let normalized = bridge.normalize_completed_text(r#"{"note":null}"#).unwrap();
        assert_eq!(normalized.text, "{}");
    }

    #[test]
    fn does_not_strip_null_that_internal_ref_allows() {
        let bridge = bridge(json!({
            "$defs":{"nullable":{"type":["string","null"]}},
            "type":"object",
            "properties":{
                "kept":{"$ref":"#/$defs/nullable"},
                "removed":{"type":"string"}
            },
            "additionalProperties":false
        }));

        let normalized = bridge
            .normalize_completed_text(r#"{"kept":null,"removed":null}"#)
            .unwrap();
        assert_eq!(normalized.text, r#"{"kept":null}"#);
    }

    #[test]
    fn refuses_to_return_candidate_that_still_violates_original_schema() {
        let bridge = bridge(json!({
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "minProperties":1,
            "additionalProperties":false
        }));

        let error = bridge
            .normalize_completed_text(r#"{"value":null}"#)
            .unwrap_err();
        assert!(matches!(
            error,
            StructuredOutputError::OriginalViolation { .. }
        ));
    }

    #[test]
    fn rejects_value_that_does_not_even_match_upstream_projection() {
        let bridge = bridge(json!({
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value"],
            "additionalProperties":false
        }));

        let error = bridge
            .normalize_completed_text(r#"{"value":42}"#)
            .unwrap_err();
        assert!(matches!(
            error,
            StructuredOutputError::UpstreamViolation { .. }
        ));
    }

    #[test]
    fn rejects_non_json_or_trailing_content_without_echoing_it() {
        let bridge = bridge(json!({"type":"object"}));
        for raw in ["```json\\n{}\\n```", "{} trailing"] {
            let error = bridge.normalize_completed_text(raw).unwrap_err();
            assert!(matches!(error, StructuredOutputError::InvalidJson { .. }));
            assert!(!error.to_string().contains(raw));
        }
    }

    #[test]
    fn rejects_external_relative_and_named_anchor_refs() {
        for reference in [
            "https://example.com/schema.json",
            "other.json#/$defs/value",
            "/tmp/schema.json",
            "#named-anchor",
            "#/%24defs/value",
        ] {
            let error = SchemaBridge::build(&json!({"$ref":reference})).unwrap_err();
            assert!(matches!(error, SchemaBridgeBuildError::Unsupported { .. }));
        }
    }

    #[test]
    fn rejects_unresolved_and_recursive_local_refs() {
        let missing = SchemaBridge::build(&json!({"$ref":"#/$defs/missing"})).unwrap_err();
        assert!(matches!(
            missing,
            SchemaBridgeBuildError::Unsupported { .. }
        ));

        let recursive = SchemaBridge::build(&json!({
            "$defs":{"node":{"$ref":"#/$defs/node"}},
            "$ref":"#/$defs/node"
        }))
        .unwrap_err();
        assert!(matches!(
            recursive,
            SchemaBridgeBuildError::Unsupported { .. }
        ));
    }

    #[test]
    fn rejects_nested_schema_resource_identifiers() {
        for keyword in ["$id", "$anchor", "$dynamicAnchor"] {
            let schema = json!({
                "$id":"https://example.com/root.json",
                "type":"object",
                "properties":{
                    "value":{
                        keyword:"nested-resource",
                        "type":"string"
                    }
                }
            });
            let error = SchemaBridge::build(&schema).unwrap_err();
            match error {
                SchemaBridgeBuildError::Unsupported { path, reason } => {
                    assert_eq!(path, format!("#/properties/value/{keyword}"));
                    assert!(reason.contains("nested schema resource identifiers"));
                }
                other => panic!("expected unsupported nested {keyword}, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_ambiguous_combinators_but_ignores_literal_annotation_data() {
        for keyword in ["allOf", "anyOf", "oneOf"] {
            let error = SchemaBridge::build(&json!({keyword:[{"type":"string"}]})).unwrap_err();
            assert!(matches!(error, SchemaBridgeBuildError::Unsupported { .. }));
        }

        let literal = json!({"anyOf":[{"$ref":"https://literal.invalid"}]});
        let bridge = SchemaBridge::build(&json!({
            "type":"object",
            "const":literal,
            "default":literal,
            "examples":[literal]
        }));
        assert!(bridge.is_ok());
    }

    #[test]
    fn rejects_structural_ref_siblings() {
        let error = SchemaBridge::build(&json!({
            "$defs":{"text":{"type":"string"}},
            "$ref":"#/$defs/text",
            "minLength":1
        }))
        .unwrap_err();
        assert!(matches!(error, SchemaBridgeBuildError::Unsupported { .. }));
    }

    #[test]
    fn rejects_invalid_original_schema_before_response_processing() {
        let error = SchemaBridge::build(&json!({"type":"not-a-json-schema-type"})).unwrap_err();
        assert!(matches!(
            error,
            SchemaBridgeBuildError::InvalidOriginal { .. }
        ));
    }

    #[test]
    fn bridge_and_compiled_validators_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SchemaBridge>();
        assert_send_sync::<jsonschema::Validator>();
    }
}
