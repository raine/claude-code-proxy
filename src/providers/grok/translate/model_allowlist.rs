use std::collections::HashMap;

use crate::registry::GROK_MODELS;

pub const GROK_DEFAULT_MODEL: &str = "grok-build-0.1";

/// Wire models the proxy will send upstream (single source: registry::GROK_MODELS).
pub fn allowed_models() -> &'static [&'static str] {
    GROK_MODELS
}

static ALIAS_TARGETS: once_cell::sync::Lazy<HashMap<&'static str, &'static str>> =
    once_cell::sync::Lazy::new(|| {
        let mut m = HashMap::new();
        for model in GROK_MODELS {
            m.insert(*model, *model);
        }
        // Short / legacy aliases → canonical wire ids
        m.insert("grok-build", GROK_DEFAULT_MODEL);
        m.insert("grok-build-latest", GROK_DEFAULT_MODEL);
        m.insert("grok-composer-2.5", "grok-composer-2.5-fast");
        m.insert("grok-4.3-latest", "grok-4.3");
        m.insert("grok-4.5-latest", "grok-4.5");
        // Anthropic-style aliases → coding default when aliasProvider=grok
        for alias in [
            "haiku",
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "sonnet",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "opus",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "fable",
            "claude-fable-5",
        ] {
            m.insert(alias, GROK_DEFAULT_MODEL);
        }
        m
    });

pub fn resolve_model(model: &str) -> String {
    ALIAS_TARGETS
        .get(model)
        .copied()
        .unwrap_or(model)
        .to_string()
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if GROK_MODELS.contains(&model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

#[derive(Debug)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

impl std::error::Error for ModelNotAllowedError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_aliases_to_default() {
        assert_eq!(resolve_model("haiku"), GROK_DEFAULT_MODEL);
        assert_eq!(resolve_model("claude-opus-4-8"), GROK_DEFAULT_MODEL);
        assert_eq!(resolve_model("grok-build"), GROK_DEFAULT_MODEL);
    }

    #[test]
    fn resolve_explicit_models() {
        assert_eq!(resolve_model("grok-4.3"), "grok-4.3");
        assert_eq!(resolve_model("grok-build-0.1"), "grok-build-0.1");
        assert_eq!(
            resolve_model("grok-composer-2.5-fast"),
            "grok-composer-2.5-fast"
        );
    }

    #[test]
    fn assert_allowed() {
        assert!(assert_allowed_model("grok-build-0.1").is_ok());
        assert!(assert_allowed_model("grok-composer-2.5-fast").is_ok());
        assert!(assert_allowed_model("gpt-5.4").is_err());
    }
}
