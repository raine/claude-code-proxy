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
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-6.1-sol",
];

pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("haiku", "gpt-6-luna"),
    ("claude-haiku-4-5", "gpt-6-luna"),
    ("claude-haiku-4-5-20251001", "gpt-6-luna"),
    ("sonnet", "gpt-5.6-terra"),
    ("claude-sonnet-4-6", "gpt-5.6-terra"),
    ("claude-sonnet-5", "gpt-5.6-terra"),
    ("opus", "gpt-6-sol"),
    ("claude-opus-4-7", "gpt-6-sol"),
    ("claude-opus-4-8", "gpt-6-sol"),
    ("claude-opus-5", "gpt-6-sol"),
    ("claude-opus-5-5", "gpt-6-sol"),
    ("fable", "gpt-6-sol"),
    ("claude-fable-5", "gpt-6-sol"),
];

#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub service_tier: Option<ServiceTier>,
}

/// Models whose Codex catalog entry offers the `ultrafast` service tier.
/// Every other model offers `priority` at most, so it gets no `-ultrafast` form.
pub const ULTRAFAST_MODELS: &[&str] = &["gpt-6-astra"];

const FAST_SUFFIX: &str = "-fast";
const ULTRAFAST_SUFFIX: &str = "-ultrafast";

/// Splits a local service-tier suffix off a registered model: `-fast` on any
/// registered model, `-ultrafast` only on [`ULTRAFAST_MODELS`].
pub fn split_tier_suffix(model: &str) -> Option<(&str, ServiceTier)> {
    if let Some(base) = model.strip_suffix(ULTRAFAST_SUFFIX) {
        return ULTRAFAST_MODELS
            .contains(&base)
            .then_some((base, ServiceTier::Ultrafast));
    }
    let base = model.strip_suffix(FAST_SUFFIX)?;
    ALLOWED_MODELS
        .contains(&base)
        .then_some((base, ServiceTier::Priority))
}

/// Local tier-suffixed names advertised next to a registered model.
pub fn tier_model_variants(model: &str) -> Vec<String> {
    let mut variants = vec![format!("{model}{FAST_SUFFIX}")];
    if ULTRAFAST_MODELS.contains(&model) {
        variants.push(format!("{model}{ULTRAFAST_SUFFIX}"));
    }
    variants
}

/// Narrows a requested tier to one `model` offers. The backend serves an
/// unlisted tier at standard speed without an error, so ultrafast on a model
/// outside [`ULTRAFAST_MODELS`] falls back to priority, the fastest tier there.
pub fn service_tier_for_model(model: &str, tier: ServiceTier) -> ServiceTier {
    match tier {
        ServiceTier::Ultrafast if !ULTRAFAST_MODELS.contains(&model) => ServiceTier::Priority,
        tier => tier,
    }
}

fn resolve_tier_model_alias(model: &str) -> ResolvedModel {
    match split_tier_suffix(model) {
        Some((base, tier)) => ResolvedModel {
            model: base.to_string(),
            service_tier: Some(tier),
        },
        None => ResolvedModel {
            model: model.to_string(),
            service_tier: None,
        },
    }
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    resolve_model_request_with_config_override(model, true)
}

pub fn resolve_model_request_with_config_override(
    model: &str,
    apply_config_override: bool,
) -> ResolvedModel {
    let override_model = apply_config_override.then(config::codex_model).flatten();
    resolve_with_model_override(model, override_model.as_deref())
}

fn resolve_with_model_override(model: &str, override_model: Option<&str>) -> ResolvedModel {
    let alias = MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| *target)
        .unwrap_or(model);

    let requested = resolve_tier_model_alias(alias);

    let resolved = match override_model {
        Some(val) if !val.is_empty() => resolve_tier_model_alias(val),
        _ => requested.clone(),
    };

    // A tier suffix on the override wins; otherwise the requested suffix
    // carries over to the override model, which may not offer that tier.
    let service_tier = resolved
        .service_tier
        .or(requested.service_tier)
        .map(|tier| service_tier_for_model(&resolved.model, tier));

    ResolvedModel {
        model: resolved.model,
        service_tier,
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
    matches!(
        model,
        "gpt-5.6-luna"
            | "gpt-5.6-sol"
            | "gpt-5.6-terra"
            | "gpt-6-astra"
            | "gpt-6-luna"
            | "gpt-6-sol"
            | "gpt-6.1-sol"
    )
}

/// Luna models exist only behind the Responses Lite lane; the full
/// Responses API resolves them to a `-free` variant and returns 404 (Model not
/// found gpt-5.6-luna-free-...). Hosted web_search requests must run on the
/// full lane, so luna is upgraded to its nearest full-lane sibling.
pub fn full_lane_web_search_model(model: &str) -> &str {
    match model {
        "gpt-5.6-luna" => "gpt-5.6-sol",
        "gpt-6-luna" => "gpt-6-sol",
        _ => model,
    }
}

pub fn is_valid_model_for_codex(model: &str) -> bool {
    if ALLOWED_MODELS.contains(&model) {
        return true;
    }
    if split_tier_suffix(model).is_some() {
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
        assert_eq!(r.model, "gpt-6-luna");
    }

    #[test]
    fn web_search_upgrades_luna_to_full_lane_sibling() {
        assert_eq!(full_lane_web_search_model("gpt-5.6-luna"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(full_lane_web_search_model("gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(full_lane_web_search_model("gpt-5.4"), "gpt-5.4");
        assert_eq!(full_lane_web_search_model("gpt-6-luna"), "gpt-6-sol");
        assert_eq!(full_lane_web_search_model("gpt-6-sol"), "gpt-6-sol");
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
        assert_eq!(r.model, "gpt-6-sol");
    }

    #[test]
    fn opus_aliases_resolve_to_sol() {
        for model in ["claude-opus-4-8", "claude-opus-5", "claude-opus-5-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn fable_5_resolves_to_sol() {
        for model in ["fable", "claude-fable-5"] {
            let r = resolve_model_request(model);
            assert_eq!(r.model, "gpt-6-sol");
        }
    }

    #[test]
    fn gpt_6_sol_fast_adds_priority() {
        for model in ["gpt-6-sol", "gpt-6.1-sol"] {
            let r = resolve_model_request(&format!("{model}-fast"));
            assert_eq!(r.model, model);
            assert_eq!(r.service_tier, Some(ServiceTier::Priority));
        }
    }

    #[test]
    fn gpt_6_models_use_responses_lite() {
        assert!(uses_responses_lite("gpt-6-sol"));
        assert!(uses_responses_lite("gpt-6-luna"));
        assert!(uses_responses_lite("gpt-6.1-sol"));
    }

    #[test]
    fn fast_suffix_adds_priority() {
        let r = resolve_model_request("gpt-5.6-sol-fast");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.service_tier, Some(ServiceTier::Priority));
    }

    #[test]
    fn ultrafast_suffix_adds_ultrafast_tier() {
        for model in ULTRAFAST_MODELS {
            let name = format!("{model}-ultrafast");
            assert!(is_valid_model_for_codex(&name), "{name}");
            let r = resolve_with_model_override(&name, None);
            assert_eq!(r.model, *model);
            assert_eq!(r.service_tier, Some(ServiceTier::Ultrafast));
            assert!(assert_allowed_model(&r.model).is_ok());
        }
    }

    #[test]
    fn ultrafast_suffix_is_rejected_for_models_without_the_tier() {
        for model in ALLOWED_MODELS {
            if ULTRAFAST_MODELS.contains(model) {
                continue;
            }
            let name = format!("{model}-ultrafast");
            assert!(!is_valid_model_for_codex(&name), "{name}");
            assert_eq!(split_tier_suffix(&name), None, "{name}");
            let r = resolve_with_model_override(&name, None);
            assert_eq!(r.service_tier, None, "{name}");
            assert!(assert_allowed_model(&r.model).is_err(), "{name}");
        }
    }

    #[test]
    fn ultrafast_models_are_registered() {
        for model in ULTRAFAST_MODELS {
            assert!(ALLOWED_MODELS.contains(model), "{model}");
        }
    }

    #[test]
    fn service_tier_narrows_to_what_the_model_offers() {
        for (model, tier, expected) in [
            (
                "gpt-6-astra",
                ServiceTier::Ultrafast,
                ServiceTier::Ultrafast,
            ),
            ("gpt-6-astra", ServiceTier::Priority, ServiceTier::Priority),
            ("gpt-6-astra", ServiceTier::Flex, ServiceTier::Flex),
            ("gpt-6-sol", ServiceTier::Ultrafast, ServiceTier::Priority),
            ("gpt-6-luna", ServiceTier::Ultrafast, ServiceTier::Priority),
            ("gpt-6-sol", ServiceTier::Priority, ServiceTier::Priority),
            ("gpt-6-sol", ServiceTier::Flex, ServiceTier::Flex),
        ] {
            assert_eq!(
                service_tier_for_model(model, tier.clone()),
                expected,
                "{model} {tier:?}"
            );
        }
    }

    #[test]
    fn tier_suffix_is_stripped_once() {
        assert_eq!(
            split_tier_suffix("gpt-6-astra-fast"),
            Some(("gpt-6-astra", ServiceTier::Priority))
        );
        assert_eq!(split_tier_suffix("gpt-6-astra-fast-fast"), None);
        assert_eq!(split_tier_suffix("gpt-6-astra-ultrafast-fast"), None);
        assert_eq!(split_tier_suffix("gpt-6-astra"), None);
        assert_eq!(split_tier_suffix("-fast"), None);
    }

    #[test]
    fn tier_model_variants_lists_supported_suffixes() {
        assert_eq!(
            tier_model_variants("gpt-6-astra"),
            ["gpt-6-astra-fast", "gpt-6-astra-ultrafast"]
        );
        assert_eq!(tier_model_variants("gpt-6-sol"), ["gpt-6-sol-fast"]);
    }

    #[test]
    fn model_override_merges_tier_suffixes() {
        let cases: &[(&str, Option<&str>, &str, Option<ServiceTier>)] = &[
            ("gpt-6-sol", None, "gpt-6-sol", None),
            ("gpt-6-sol", Some(""), "gpt-6-sol", None),
            (
                "gpt-6-sol-fast",
                None,
                "gpt-6-sol",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-sol",
                Some("gpt-6-astra-fast"),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-sol-fast",
                Some("gpt-6-astra"),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-sol",
                Some("gpt-6-astra-ultrafast"),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                "gpt-6-sol-fast",
                Some("gpt-6-astra-ultrafast"),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                "gpt-6-astra-ultrafast",
                Some("gpt-6-astra-fast"),
                "gpt-6-astra",
                Some(ServiceTier::Priority),
            ),
            (
                "gpt-6-astra-ultrafast",
                Some("gpt-6-astra"),
                "gpt-6-astra",
                Some(ServiceTier::Ultrafast),
            ),
            (
                "gpt-6-astra-ultrafast",
                Some("gpt-6-sol"),
                "gpt-6-sol",
                Some(ServiceTier::Priority),
            ),
            ("opus", Some("gpt-6-astra"), "gpt-6-astra", None),
        ];
        for (requested, override_model, model, tier) in cases {
            let r = resolve_with_model_override(requested, *override_model);
            assert_eq!(r.model, *model, "{requested} + {override_model:?}");
            assert_eq!(&r.service_tier, tier, "{requested} + {override_model:?}");
        }
    }

    #[test]
    fn allowed_models_accept_base() {
        assert!(assert_allowed_model("gpt-5.4").is_ok());
        assert!(assert_allowed_model("gpt-5.6-sol").is_ok());
        assert!(assert_allowed_model("gpt-5.6-terra").is_ok());
        assert!(assert_allowed_model("gpt-6-astra").is_ok());
        assert!(assert_allowed_model("gpt-5.6-luna").is_ok());
        assert!(assert_allowed_model("gpt-6.1-sol").is_ok());
    }

    #[test]
    fn not_allowed_rejected() {
        assert!(assert_allowed_model("gpt-7").is_err());
    }
}
