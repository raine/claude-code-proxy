use crate::config;
use crate::registry::CODEX_MODELS;

use super::request::ServiceTier;

pub const ALLOWED_MODELS: &[&str] = CODEX_MODELS;

pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("haiku", "gpt-5.6-luna"),
    ("claude-haiku-4-5", "gpt-5.6-luna"),
    ("claude-haiku-4-5-20251001", "gpt-5.6-luna"),
    ("sonnet", "gpt-5.6-terra"),
    ("claude-sonnet-4-6", "gpt-5.6-terra"),
    ("claude-sonnet-5", "gpt-5.6-terra"),
    ("opus", "gpt-5.6-sol"),
    ("claude-opus-4-7", "gpt-5.6-sol"),
    ("claude-opus-4-8", "gpt-5.6-sol"),
    ("claude-opus-5", "gpt-5.6-sol"),
    ("fable", "gpt-5.6-sol"),
    ("claude-fable-5", "gpt-5.6-sol"),
];

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

fn fast_model_base(model: &str) -> Option<&str> {
    model
        .strip_suffix("-fast")
        .filter(|base| ALLOWED_MODELS.contains(base))
}

fn resolve_fast_model_alias(model: &str) -> ResolvedModel {
    if let Some(base) = fast_model_base(model) {
        ResolvedModel {
            model: base.to_string(),
            service_tier: Some(ServiceTier::Priority),
        }
    } else {
        ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        }
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    let alias = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| *target)
        .unwrap_or(model);

    let requested = resolve_fast_model_alias(alias);

    let override_model = config::codex_model();
    let resolved = match override_model {
        Some(ref val) if !val.is_empty() => resolve_fast_model_alias(val),
        _ => requested.clone(),
    };

    ResolvedModel {
        model: resolved.model,
        service_tier: if requested.service_tier == Some(ServiceTier::Priority)
            || resolved.service_tier == Some(ServiceTier::Priority)
        {
            Some(ServiceTier::Priority)
        } else {
            resolved.service_tier
        },
    }
}

pub fn resolve_model(model: &str) -> String {
    resolve_model_request(model).model
}

#[derive(Debug, Clone)]
pub struct ModelNotAllowedError {
    pub model: String,
}

impl std::fmt::Display for ModelNotAllowedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model not allowed: {}", self.model)
    }
}

pub fn assert_allowed_model(model: &str) -> Result<(), ModelNotAllowedError> {
    if ALLOWED_MODELS.contains(&model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

pub fn uses_responses_lite(model: &str) -> bool {
    matches!(model, "gpt-5.6-luna" | "gpt-5.6-sol" | "gpt-5.6-terra")
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    ALLOWED_MODELS.contains(&model)
        || fast_model_base(model).is_some()
        || MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haiku_resolves_to_luna() {
        let r = resolve_model_request("haiku");
        assert_eq!(r.model, "gpt-5.6-luna");
    }

    #[test]
    fn sonnet_resolves_to_terra() {
        let r = resolve_model_request("sonnet");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn sonnet_5_resolves_to_terra() {
        let r = resolve_model_request("claude-sonnet-5");
        assert_eq!(r.model, "gpt-5.6-terra");
    }

    #[test]
    fn opus_resolves_to_sol() {
        let r = resolve_model_request("opus");
        assert_eq!(r.model, "gpt-5.6-sol");
    }

    #[test]
    fn opus_4_8_resolves_to_sol() {
        let r = resolve_model_request("claude-opus-4-8");
        assert_eq!(r.model, "gpt-5.6-sol");
    }

    #[test]
    fn opus_5_resolves_to_sol() {
        let r = resolve_model_request("claude-opus-5");
        assert_eq!(r.model, "gpt-5.6-sol");
    }

    #[test]
    fn fable_5_resolves_to_sol() {
        for model in ["fable", "claude-fable-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-5.6-sol");
        }
    }

    #[test]
    fn fast_suffix_adds_priority() {
        let r = resolve_model_request("gpt-5.6-sol-fast");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn repeated_fast_suffixes_remain_unresolved() {
        let resolved = resolve_model_request("gpt-5.6-sol-fast-fast");
        assert_eq!(resolved.model, "gpt-5.6-sol-fast-fast");
        assert_eq!(resolved.service_tier, None);
        assert!(!is_valid_model_for_codex("gpt-5.6-sol-fast-fast"));
        assert!(!is_valid_model_for_codex("unknown-fast-fast"));
    }

    #[test]
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("gpt-7").is_err());
    }
}
