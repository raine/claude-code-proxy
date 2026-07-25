#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub model: String,
    pub reasoning_effort: Option<&'static str>,
}

pub fn resolve_model_request(model: &str) -> ResolvedModel {
    match model {
        "grok-4.5-medium" => ResolvedModel {
            model: "grok-4.5".into(),
            reasoning_effort: Some("medium"),
        },
        "grok-4.5-high" => ResolvedModel {
            model: "grok-4.5".into(),
            reasoning_effort: Some("high"),
        },
        // Claude Code may request the Anthropic Opus 5 id; map it to the
        // base Grok 4.5 wire model so /effort remains effective.
        "claude-opus-5" => ResolvedModel {
            model: "grok-4.5".into(),
            reasoning_effort: None,
        },
        _ => ResolvedModel {
            model: model.into(),
            reasoning_effort: None,
        },
    }
}

pub fn assert_allowed_model(model: &str) -> anyhow::Result<()> {
    if matches!(model, "grok-composer-2.5-fast" | "grok-4.5") {
        Ok(())
    } else {
        anyhow::bail!("unsupported Grok model")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_effort_alias_resolves_to_grok_4_5() {
        assert_eq!(
            resolve_model_request("grok-4.5-high"),
            ResolvedModel {
                model: "grok-4.5".into(),
                reasoning_effort: Some("high"),
            }
        );
    }

    #[test]
    fn medium_effort_alias_resolves_to_grok_4_5() {
        assert_eq!(
            resolve_model_request("grok-4.5-medium"),
            ResolvedModel {
                model: "grok-4.5".into(),
                reasoning_effort: Some("medium"),
            }
        );
    }

    #[test]
    fn regular_model_does_not_override_effort() {
        assert_eq!(
            resolve_model_request("grok-4.5"),
            ResolvedModel {
                model: "grok-4.5".into(),
                reasoning_effort: None,
            }
        );
    }

    #[test]
    fn opus_5_alias_resolves_to_grok_4_5() {
        assert_eq!(
            resolve_model_request("claude-opus-5"),
            ResolvedModel {
                model: "grok-4.5".into(),
                reasoning_effort: None,
            }
        );
    }
}
