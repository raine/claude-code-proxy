use std::collections::HashSet;

use crate::config;

use super::model_catalog::{catalog_model, catalog_models};
use super::request::ServiceTier;

/// Baseline allowlist compiled into the binary. At runtime the Codex CLI's
/// model cache (`model_catalog`) extends it, so models OpenAI ships to Codex
/// work before a proxy release lists them.
pub const ALLOWED_MODELS: &[&str] = &[
    "gpt-5.2",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
];

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

/// True for baseline models and for API-supported models from the Codex
/// model cache.
pub fn is_allowed_model(model: &str) -> bool {
    ALLOWED_MODELS.contains(&model)
        || catalog_model(model).is_some_and(|entry| entry.supported_in_api)
}

/// Baseline ∪ API-supported catalog models, sorted and de-duplicated —
/// for listings and error messages.
pub fn allowed_models() -> Vec<String> {
    let mut set: HashSet<String> = ALLOWED_MODELS.iter().map(|m| (*m).to_string()).collect();
    for entry in catalog_models() {
        if entry.supported_in_api {
            set.insert(entry.slug);
        }
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort_unstable();
    out
}

/// Catalog models advertised by the Codex picker (`visibility == "list"`)
/// and usable through the API — what `models` and the unknown-model hint
/// should show in addition to the baseline.
pub fn listed_catalog_models() -> Vec<String> {
    catalog_models()
        .into_iter()
        .filter(|entry| entry.listed && entry.supported_in_api)
        .map(|entry| entry.slug)
        .collect()
}

pub fn allowed_models_display() -> String {
    allowed_models().join(", ")
}

fn fast_model_aliases() -> HashSet<String> {
    allowed_models()
        .iter()
        .map(|m| format!("{m}-fast"))
        .collect()
}

fn resolve_fast_model_alias(model: &str) -> ResolvedModel {
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        let base = model.trim_end_matches("-fast");
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
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let alias = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| *target)
        .unwrap_or(model);

    let requested = resolve_fast_model_alias(alias);

    let override_model = apply_config_override.then(config::codex_model).flatten();
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
    if is_allowed_model(model) {
        Ok(())
    } else {
        Err(ModelNotAllowedError {
            model: model.to_string(),
        })
    }
}

pub fn uses_responses_lite(model: &str) -> bool {
    matches!(
        model,
        "gpt-5.6-luna" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-6-astra"
    ) || catalog_model(model).is_some_and(|entry| entry.use_responses_lite)
}

/// `gpt-5.6-luna` exists only behind the Responses Lite lane; the full
/// Responses API resolves it to a `-free` variant and returns 404 (Model not
/// found gpt-5.6-luna-free-...). Hosted web_search requests must run on the
/// full lane, so luna is upgraded to its nearest full-lane sibling.
pub fn full_lane_web_search_model(model: &str) -> &str {
    if model == "gpt-5.6-luna" {
        "gpt-5.6-sol"
    } else {
        model
    }
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    if is_allowed_model(model) {
        return true;
    }
    let fast_set = fast_model_aliases();
    if fast_set.contains(model) {
        return true;
    }
    MODEL_ALIASES.iter().any(|(alias, _)| *alias == model)
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
    fn web_search_upgrades_luna_to_full_lane_sibling() {
        assert_eq!(full_lane_web_search_model("gpt-5.6-luna"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(full_lane_web_search_model("gpt-5.4"), "gpt-5.4");
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
    fn opus_aliases_resolve_to_sol() {
        for model in ["claude-opus-4-8", "claude-opus-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-5.6-sol");
        }
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
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-6-astra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("gpt-7").is_err());
        assert!(assert_allowed_model("gpt-7-fast").is_err());
    }

    #[test]
    fn allowed_models_listing_contains_baseline() {
        let listing = allowed_models();
        for baseline in ALLOWED_MODELS {
            assert!(listing.iter().any(|m| m == baseline), "{baseline} missing");
        }
        assert!(allowed_models_display().contains("gpt-6-astra"));
    }
}
