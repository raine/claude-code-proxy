//! Model id resolution for the generic Anthropic-compatible upstream.
//!
//! Public catalog ids use a configurable prefix (default `merge:`). The
//! upstream Messages API receives the id with the prefix stripped.

use crate::registry::MERGE_MODELS;

pub const DEFAULT_PREFIX: &str = "merge:";

/// Resolve a Claude Code model id into the upstream Anthropic model slug.
///
/// Accepts:
/// - Prefixed ids: `merge:anthropic/claude-sonnet-5` → `anthropic/claude-sonnet-5`
/// - Bare catalog ids listed in [`MERGE_MODELS`] (already prefixed)
pub fn resolve_upstream_model(requested: &str) -> Result<String, String> {
    let stripped = strip_prefix(requested);
    if !is_allowed_upstream(&stripped) {
        return Err(format!(
            "model \"{requested}\" is not in the Anthropic-compatible catalog"
        ));
    }
    Ok(stripped)
}

pub fn strip_prefix(model: &str) -> String {
    if let Some(rest) = model.strip_prefix(DEFAULT_PREFIX) {
        rest.to_string()
    } else {
        model.to_string()
    }
}

pub fn catalog_models() -> Vec<String> {
    MERGE_MODELS
        .iter()
        .map(|model| {
            if model.starts_with(DEFAULT_PREFIX) {
                (*model).to_string()
            } else {
                format!("{DEFAULT_PREFIX}{model}")
            }
        })
        .collect()
}

fn is_allowed_upstream(upstream: &str) -> bool {
    MERGE_MODELS.iter().any(|entry| {
        let catalog_upstream = strip_prefix(entry);
        catalog_upstream == upstream
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_merge_prefix() {
        assert_eq!(
            resolve_upstream_model("merge:anthropic/claude-sonnet-5").unwrap(),
            "anthropic/claude-sonnet-5"
        );
    }

    #[test]
    fn rejects_unknown_slug() {
        assert!(resolve_upstream_model("merge:anthropic/gpt-4o").is_err());
        assert!(resolve_upstream_model("merge:openai/gpt-5").is_err());
    }

    #[test]
    fn catalog_is_prefixed() {
        let models = catalog_models();
        assert!(
            models
                .iter()
                .all(|m| m.starts_with(DEFAULT_PREFIX) && m.contains("anthropic/"))
        );
        assert!(
            models
                .iter()
                .any(|m| m == "merge:anthropic/claude-opus-4-8")
        );
        assert!(models.iter().any(|m| m == "merge:anthropic/fable-5"));
        assert!(
            models
                .iter()
                .any(|m| m == "merge:anthropic/claude-sonnet-5")
        );
        assert!(
            models
                .iter()
                .any(|m| m == "merge:anthropic/claude-haiku-4-5-20251001")
        );
    }
}
