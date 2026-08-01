/// Cursor model catalog resolves incoming model names to Cursor wire IDs.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAgentMode {
    Agent,
    Plan,
    Ask,
}

const CURSOR_ROUTING_PREFIXES: [(&str, CursorAgentMode); 4] = [
    ("cursor-agent:", CursorAgentMode::Agent),
    ("cursor-plan:", CursorAgentMode::Plan),
    ("cursor-ask:", CursorAgentMode::Ask),
    ("cursor:", CursorAgentMode::Agent),
];

impl CursorAgentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CursorAgentMode::Agent => "AGENT_MODE_AGENT",
            CursorAgentMode::Plan => "AGENT_MODE_PLAN",
            CursorAgentMode::Ask => "AGENT_MODE_ASK",
        }
    }
}

fn resolve_wire_model(raw: &str) -> (String, bool) {
    raw.strip_suffix("-fast")
        .map(|model| (model.to_string(), true))
        .unwrap_or_else(|| (raw.to_string(), false))
}

pub(crate) fn strip_cursor_routing_prefix(model: &str) -> &str {
    CURSOR_ROUTING_PREFIXES
        .iter()
        .find_map(|(prefix, _)| model.strip_prefix(prefix))
        .unwrap_or(model)
}

pub fn resolve_cursor_model(model: &str) -> Result<CursorModelResolution, String> {
    let model = model.trim();
    for &(prefix, mode) in &CURSOR_ROUTING_PREFIXES {
        if let Some(raw) = model.strip_prefix(prefix) {
            let (model_id, fast) = resolve_wire_model(raw);
            return Ok(CursorModelResolution {
                model_id,
                mode,
                fast,
            });
        }
    }

    let (model_id, mode, fast) = match model {
        "cursor" | "cursor-agent" => ("default", CursorAgentMode::Agent, false),
        "cursor-plan" => ("default", CursorAgentMode::Plan, false),
        "cursor-ask" => ("default", CursorAgentMode::Ask, false),
        "cursor-composer" | "composer-2.5" => ("composer-2.5", CursorAgentMode::Agent, false),
        "cursor-composer-fast" | "composer-2.5-fast" => {
            ("composer-2.5", CursorAgentMode::Agent, true)
        }
        _ => {
            return Err(format!(
                "unknown cursor model: {model}. Supported: cursor:<id>, cursor-plan:<id>, cursor-ask:<id>, cursor-agent"
            ));
        }
    };
    Ok(CursorModelResolution {
        model_id: model_id.to_string(),
        mode,
        fast,
    })
}

#[derive(Debug, Clone)]
pub struct CursorModelResolution {
    pub model_id: String,
    pub mode: CursorAgentMode,
    pub fast: bool,
}

pub fn cursor_supported_models() -> Vec<String> {
    let mut out: Vec<String> = CURSOR_LEGACY_MODELS
        .iter()
        .map(|model| (*model).to_string())
        .collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_legacy_cursor() {
        let resolution = resolve_cursor_model("cursor").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_legacy_cursor_agent() {
        let resolution = resolve_cursor_model("cursor-agent").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_legacy_cursor_plan() {
        let resolution = resolve_cursor_model("cursor-plan").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Plan);
    }

    #[test]
    fn resolve_legacy_cursor_ask() {
        let resolution = resolve_cursor_model("cursor-ask").unwrap();
        assert_eq!(resolution.model_id, "default");
        assert_eq!(resolution.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn resolve_prefixed_cursor() {
        let resolution = resolve_cursor_model("cursor:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_prefixed_cursor_plan() {
        let resolution = resolve_cursor_model("cursor-plan:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Plan);
    }

    #[test]
    fn resolve_prefixed_cursor_ask() {
        let resolution = resolve_cursor_model("cursor-ask:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Ask);
    }

    #[test]
    fn resolve_prefixed_cursor_agent() {
        let resolution = resolve_cursor_model("cursor-agent:gpt-5.5").unwrap();
        assert_eq!(resolution.model_id, "gpt-5.5");
        assert_eq!(resolution.mode, CursorAgentMode::Agent);
    }

    #[test]
    fn resolve_unknown_model_errors() {
        assert!(resolve_cursor_model("unknown-model").is_err());
    }

    #[test]
    fn resolve_composer_models() {
        let regular = resolve_cursor_model("composer-2.5").unwrap();
        assert_eq!(regular.mode, CursorAgentMode::Agent);
        assert_eq!(regular.model_id, "composer-2.5");
        assert!(!regular.fast);

        let fast = resolve_cursor_model("composer-2.5-fast").unwrap();
        assert_eq!(fast.mode, CursorAgentMode::Agent);
        assert_eq!(fast.model_id, "composer-2.5");
        assert!(fast.fast);
    }

    #[test]
    fn supported_models_includes_all_legacy() {
        let models = cursor_supported_models();
        for model in CURSOR_LEGACY_MODELS {
            assert!(models.contains(&model.to_string()), "missing {model}");
        }
    }
}
