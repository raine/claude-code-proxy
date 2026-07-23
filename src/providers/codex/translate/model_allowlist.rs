use std::collections::HashSet;

use crate::config;

use super::request::ServiceTier;

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
    ("fable", "gpt-5.6-sol"),
    ("claude-fable-5", "gpt-5.6-sol"),
];

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

fn fast_model_aliases() -> HashSet<String> {
    ALLOWED_MODELS.iter().map(|m| format!("{m}-fast")).collect()
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

/// The gpt-5.6 family defaults to the Responses Lite lane. The lite lane
/// requires `parallel_tool_calls: false` (the backend rejects the request
/// with 400 `unsupported_value` otherwise), so every tool call is serialized
/// into its own assistant turn there. `gpt-5.6-sol` and `gpt-5.6-terra` also
/// exist on the full Responses lane, where parallel tool calls work;
/// `codex.fullLane` / `CCP_CODEX_FULL_LANE` opts them into it. `gpt-5.6-luna`
/// stays on the lite lane unconditionally — see [`full_lane_web_search_model`].
pub fn uses_responses_lite(model: &str) -> bool {
    uses_responses_lite_with_full_lane(model, config::codex_full_lane())
}

fn uses_responses_lite_with_full_lane(model: &str, full_lane: bool) -> bool {
    if model == "gpt-5.6-luna" {
        return true;
    }
    if full_lane {
        return false;
    }
    matches!(model, "gpt-5.6-sol" | "gpt-5.6-terra")
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
    if ALLOWED_MODELS.contains(&model) {
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
    fn responses_lite_defaults_for_56_family_only() {
        for model in ["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"] {
            assert!(uses_responses_lite_with_full_lane(model, false));
        }
        for model in ["gpt-5.3-codex", "gpt-5.4", "gpt-5.5"] {
            assert!(!uses_responses_lite_with_full_lane(model, false));
        }
    }

    #[test]
    fn full_lane_opts_sol_and_terra_out_of_lite_but_never_luna() {
        assert!(!uses_responses_lite_with_full_lane("gpt-5.6-sol", true));
        assert!(!uses_responses_lite_with_full_lane("gpt-5.6-terra", true));
        assert!(uses_responses_lite_with_full_lane("gpt-5.6-luna", true));
        assert!(!uses_responses_lite_with_full_lane("gpt-5.4", true));
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
    fn opus_4_8_resolves_to_sol() {
        let r = resolve_model_request("claude-opus-4-8");
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
