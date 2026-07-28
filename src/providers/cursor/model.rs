/// Cursor model catalog -- resolves incoming model names to Cursor model IDs.
///
/// Resolution rules:
/// - `cursor:`, `cursor-plan:`, `cursor-ask:` prefixes are stripped and mapped
///   to the corresponding agent mode.
/// - Legacy names like `cursor`, `cursor-agent`, `cursor-composer`,
///   `cursor-composer-fast`, `cursor-plan`, `cursor-ask`, `composer-2.5`,
///   `composer-2.5-fast` are recognized.
/// - `cursor-agent:` is also supported for agent mode routing.
pub const CURSOR_LEGACY_MODELS: &[&str] = &[
    "cursor",
    "cursor-agent",
    "cursor-composer",
    "cursor-composer-fast",
    "cursor-plan",
    "cursor-ask",
    "composer-2.5",
    "composer-2.5-fast",
];

/// Agent mode derived from model prefix or name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAgentMode {
    Agent,
    Plan,
    Ask,
}

impl CursorAgentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CursorAgentMode::Agent => "AGENT_MODE_AGENT",
            CursorAgentMode::Plan => "AGENT_MODE_PLAN",
            CursorAgentMode::Ask => "AGENT_MODE_ASK",
        }
    }
}

/// Resolve a model string into a (model_id, mode) pair.
///
/// Returns an error if the model is not recognized.
pub fn resolve_cursor_model(model: &str) -> Result<CursorModelResolution, String> {
    let model = model.trim();

    // Strip known prefixes
    if let Some(rest) = model.strip_prefix("cursor-agent:") {
        return Ok(CursorModelResolution {
            model_id: rest.to_string(),
            mode: CursorAgentMode::Agent,
        });
    }
    if let Some(rest) = model.strip_prefix("cursor-plan:") {
        return Ok(CursorModelResolution {
            model_id: rest.to_string(),
            mode: CursorAgentMode::Plan,
        });
    }
    if let Some(rest) = model.strip_prefix("cursor-ask:") {
        return Ok(CursorModelResolution {
            model_id: rest.to_string(),
            mode: CursorAgentMode::Ask,
        });
    }
    if let Some(rest) = model.strip_prefix("cursor:") {
        return Ok(CursorModelResolution {
            model_id: rest.to_string(),
            mode: CursorAgentMode::Agent,
        });
    }

    // Legacy exact names
    match model {
        "cursor" | "cursor-agent" | "cursor-composer" | "cursor-composer-fast" => {
            Ok(CursorModelResolution {
                model_id: model.to_string(),
                mode: CursorAgentMode::Agent,
            })
        }
        "cursor-plan" | "composer-2.5" => Ok(CursorModelResolution {
            model_id: model.to_string(),
            mode: CursorAgentMode::Plan,
        }),
        "cursor-ask" | "composer-2.5-fast" => Ok(CursorModelResolution {
            model_id: model.to_string(),
            mode: CursorAgentMode::Ask,
        }),
        _ => Err(format!(
            "unknown cursor model: {model}. Supported: cursor:<id>, cursor-plan:<id>, cursor-ask:<id>, cursor-agent"
        )),
    }
}

#[derive(Debug, Clone)]
pub struct CursorModelResolution {
    pub model_id: String,
    pub mode: CursorAgentMode,
}

/// Build the list of supported Cursor model names.
pub fn cursor_supported_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_legacy_cursor() {
        let r = resolve_cursor_model("cursor").unwrap();
        assert_eq!(r.model_id, "cursor");
        assert_eq!(r.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_legacy_cursor_agent() {
        let r = resolve_cursor_model("cursor-agent").unwrap();
        assert_eq!(r.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_legacy_cursor_plan() {
        let r = resolve_cursor_model("cursor-plan").unwrap();
        assert_eq!(r.mode, CursorAgentMode::Plan);
    }

    #[test]
    fn resolve_legacy_cursor_ask() {
        let r = resolve_cursor_model("cursor-ask").unwrap();
        assert_eq!(r.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn resolve_prefixed_cursor() {
        let r = resolve_cursor_model("cursor:gpt-5.5").unwrap();
        assert_eq!(r.model_id, "gpt-5.5");
        assert_eq!(r.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_prefixed_cursor_plan() {
        let r = resolve_cursor_model("cursor-plan:gpt-5.5").unwrap();
        assert_eq!(r.model_id, "gpt-5.5");
        assert_eq!(r.mode, CursorAgentMode::Plan);
    }

    #[test]
    fn resolve_prefixed_cursor_ask() {
        let r = resolve_cursor_model("cursor-ask:gpt-5.5").unwrap();
        assert_eq!(r.model_id, "gpt-5.5");
        assert_eq!(r.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn resolve_prefixed_cursor_agent() {
        let r = resolve_cursor_model("cursor-agent:gpt-5.5").unwrap();
        assert_eq!(r.model_id, "gpt-5.5");
        assert_eq!(r.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_unknown_model_errors() {
        let r = resolve_cursor_model("unknown-model");
        assert!(r.is_err());
    }

    #[test]
    fn resolve_composer_models() {
        let r = resolve_cursor_model("composer-2.5").unwrap();
        assert_eq!(r.mode, CursorAgentMode::Plan);

        let r = resolve_cursor_model("composer-2.5-fast").unwrap();
        assert_eq!(r.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn supported_models_includes_all_legacy() {
        let models = cursor_supported_models();
        for m in CURSOR_LEGACY_MODELS {
            assert!(models.contains(&m.to_string()), "missing {m}");
        }
    }
}
