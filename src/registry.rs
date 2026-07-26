use crate::{config::AliasProvider, provider::Provider};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub const ANTHROPIC_STYLE_ALIASES: &[&str] = &[
    "haiku",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "sonnet",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "opus",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "fable",
    "claude-fable-5",
];

pub(crate) const CODEX_MODELS: &[&str] = &[
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

pub(crate) const GROK_MODELS: &[&str] = &[
    "grok-composer-2.5-fast",
    "grok-4.5",
    "grok-4.5-medium",
    "grok-4.5-high",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAffinityProvider {
    Codex,
    Grok,
}

impl SessionAffinityProvider {
    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }

    fn supports_alias(self, model: &str) -> bool {
        match self {
            Self::Codex => is_anthropic_alias(model),
            Self::Grok => model == "claude-opus-5",
        }
    }
}

pub struct Registry {
    alias_provider: AliasProvider,
    models: BTreeMap<String, Vec<String>>,
    handlers: BTreeMap<String, Arc<dyn Provider>>,
}

impl Registry {
    pub fn new(alias_provider: AliasProvider) -> Self {
        let mut models: BTreeMap<String, Vec<String>> = BTreeMap::new();
        models.insert("codex".into(), expand_codex_models());
        models.insert(
            "grok".into(),
            GROK_MODELS
                .iter()
                .map(|model| (*model).to_string())
                .collect(),
        );

        let mut handlers = BTreeMap::new();
        for name in ["codex", "kimi", "cursor", "grok"] {
            let handler: Arc<dyn Provider> = match name {
                "codex" => Arc::new(crate::providers::codex::CodexProvider::new()),
                "kimi" => Arc::new(crate::providers::kimi::KimiProvider::new()),
                "cursor" => Arc::new(crate::providers::cursor::CursorProvider::new()),
                "grok" => Arc::new(crate::providers::grok::GrokProvider::new()),
                _ => unreachable!("provider list is exhaustive"),
            };
            handlers.insert(name.to_string(), handler);
        }

        Self {
            alias_provider,
            models,
            handlers,
        }
    }

    pub fn with_default_alias() -> Self {
        Self::new(crate::config::alias_provider())
    }

    pub fn list_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.models.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.handlers.get(name).cloned()
    }

    pub fn supported_models_for(&self, provider: &str) -> Vec<String> {
        let mut models = self.models.get(provider).cloned().unwrap_or_default();
        if self.models.contains_key(provider) && provider == self.alias_provider.as_str() {
            for alias in ANTHROPIC_STYLE_ALIASES {
                if !models.iter().any(|value| value == alias) {
                    models.push((*alias).to_string());
                }
            }
        }
        models.sort_unstable();
        models
    }

    pub fn all_supported_models(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in self.models.keys() {
            for model in self.supported_models_for(provider) {
                out.push((model, provider.clone()));
            }
        }
        out
    }

    pub fn grouped_models(&self) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for provider in self.models.keys() {
            out.insert(provider.clone(), self.supported_models_for(provider));
        }
        out
    }

    pub fn provider_for_model(
        &self,
        raw_model: &str,
        session_affinity: Option<&SessionAffinityProvider>,
    ) -> Option<Arc<dyn Provider>> {
        let normalized = normalize_incoming_model(raw_model);
        if is_anthropic_alias(&normalized) {
            let target = session_affinity
                .copied()
                .filter(|provider| provider.supports_alias(&normalized))
                .map(SessionAffinityProvider::as_str)
                .unwrap_or_else(|| self.alias_provider.as_str());
            if !self.models.contains_key(target) {
                return None;
            }
            return self.handlers.get(target).cloned();
        }
        for (name, models) in &self.models {
            if models.iter().any(|candidate| candidate == &normalized) {
                return self.handlers.get(name).cloned();
            }
        }

        None
    }

    pub fn unknown_model_message(&self) -> String {
        let mut parts = Vec::new();
        for (provider, models) in self.grouped_models() {
            let mut models = models;
            models.sort_unstable();
            parts.push(format!("{}: {}", provider, models.join(", ")));
        }
        format!("Supported: {}.", parts.join("; "))
    }
}

pub fn normalize_incoming_model(model: &str) -> String {
    let suffix = "[1m]";
    if model.len() >= suffix.len() && model.to_ascii_lowercase().ends_with(suffix) {
        return model[..model.len() - suffix.len()].to_string();
    }
    model.to_string()
}

pub fn is_anthropic_alias(model: &str) -> bool {
    ANTHROPIC_STYLE_ALIASES.contains(&model)
}

fn expand_codex_models() -> Vec<String> {
    let mut set = HashSet::new();
    let mut out = Vec::new();
    for model in CODEX_MODELS {
        if set.insert((*model).to_string()) {
            out.push((*model).to_string());
        }
        let fast = format!("{model}-fast");
        if set.insert(fast.clone()) {
            out.push(fast);
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_trims_hint() {
        assert_eq!(normalize_incoming_model("gpt-5.4-fast[1m]"), "gpt-5.4-fast");
        assert_eq!(normalize_incoming_model("gpt-5.4-fast"), "gpt-5.4-fast");
    }

    #[test]
    fn alias_routes_to_codex_by_default() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("haiku", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn legacy_kimi_alias_configuration_fails_closed() {
        let registry = Registry::new(AliasProvider::Kimi);
        for model in ["sonnet", "opus", "claude-opus-5"] {
            assert!(
                registry.provider_for_model(model, None).is_none(),
                "{model} must not route through disabled Kimi"
            );
        }
        assert!(registry.supported_models_for("kimi").is_empty());
    }

    #[test]
    fn opus_4_8_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-4-8", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn opus_5_routes_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        let p = registry.provider_for_model("claude-opus-5", None);
        assert!(p.is_some());
        assert_eq!(p.expect("provider").name(), "codex");
    }

    #[test]
    fn opus_5_routes_to_grok_session_affinity() {
        let registry = Registry::new(AliasProvider::Codex);
        let provider =
            registry.provider_for_model("claude-opus-5", Some(&SessionAffinityProvider::Grok));
        assert_eq!(provider.expect("provider").name(), "grok");
    }

    #[test]
    fn unsupported_grok_alias_falls_back_to_codex() {
        let registry = Registry::new(AliasProvider::Codex);
        let codex = registry.provider_for_model("sonnet", Some(&SessionAffinityProvider::Grok));
        assert_eq!(codex.expect("provider").name(), "codex");
    }

    #[test]
    fn claude_5_aliases_route_to_configured_provider() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "fable",
            "claude-fable-5",
        ] {
            let p = registry.provider_for_model(model, None);
            assert!(p.is_some(), "{model} should route to a provider");
            assert_eq!(p.expect("provider").name(), "codex");
        }
    }

    #[test]
    fn grok_effort_alias_routes_to_grok() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in ["grok-4.5-medium", "grok-4.5-high"] {
            let provider = registry.provider_for_model(model, None);
            assert_eq!(provider.expect("provider").name(), "grok");
        }
    }

    #[test]
    fn kimi_and_cursor_models_are_not_routable_or_catalogued() {
        let registry = Registry::new(AliasProvider::Codex);
        for model in [
            "kimi-for-coding",
            "kimi-k2.6",
            "k2.6",
            "kimi-for-coding[1m]",
            "cursor",
            "cursor-agent",
            "cursor-composer",
            "cursor-composer-fast",
            "cursor-plan",
            "cursor-ask",
            "composer-2.5",
            "composer-2.5-fast",
            "cursor:gpt-5.5",
            "cursor-plan:gpt-5.5",
            "cursor-ask:gpt-5.5",
        ] {
            assert!(
                registry.provider_for_model(model, None).is_none(),
                "{model} must not route"
            );
        }

        let grouped = registry.grouped_models();
        assert_eq!(
            grouped.keys().map(String::as_str).collect::<Vec<_>>(),
            ["codex", "grok"]
        );
        assert_eq!(registry.list_provider_names(), ["codex", "grok"]);

        // Provider implementations remain registered for their existing auth CLI.
        assert!(registry.provider("kimi").is_some());
        assert!(registry.provider("cursor").is_some());
    }
}
