pub const MODEL_PREFIX: &str = "opencode-go/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    ChatCompletions,
    Messages,
    Responses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: &'static str,
    pub endpoint: EndpointKind,
}

pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "grok-4.5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "gpt-5.6-luna",
        endpoint: EndpointKind::Responses,
    },
    ModelSpec {
        id: "glm-5.2",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5.1",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "glm-5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k3",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.7-code",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.6",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "kimi-k2.5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "deepseek-v4-flash",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.5",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "mimo-v2.5-pro",
        endpoint: EndpointKind::ChatCompletions,
    },
    ModelSpec {
        id: "minimax-m3",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "minimax-m2.7",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "minimax-m2.5",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.8-max",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.7-max",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.7-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.6-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "qwen3.5-plus",
        endpoint: EndpointKind::Messages,
    },
    ModelSpec {
        id: "hy3",
        endpoint: EndpointKind::ChatCompletions,
    },
];

pub fn resolve(raw: &str) -> Option<ModelSpec> {
    let id = raw.strip_prefix(MODEL_PREFIX).unwrap_or(raw);
    MODELS.iter().copied().find(|model| model.id == id)
}

pub fn advertised_models() -> Vec<String> {
    let mut result = Vec::with_capacity(MODELS.len() * 2);
    for model in MODELS {
        if !matches!(
            model.id,
            "gpt-5.6-luna" | "grok-4.5" | "kimi-k3" | "kimi-k2.6"
        ) {
            result.push(model.id.to_string());
        }
        result.push(format!("{MODEL_PREFIX}{}", model.id));
    }
    result.sort_unstable();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_catalog_is_partitioned_by_wire_protocol() {
        let chat = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::ChatCompletions)
            .count();
        let messages = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::Messages)
            .count();
        let responses = MODELS
            .iter()
            .filter(|model| model.endpoint == EndpointKind::Responses)
            .count();
        assert_eq!(chat, 13);
        assert_eq!(messages, 8);
        assert_eq!(responses, 1);
    }

    #[test]
    fn refreshed_models_resolve_and_are_advertised() {
        let advertised = advertised_models();
        for (id, endpoint) in [
            ("glm-5", EndpointKind::ChatCompletions),
            ("kimi-k2.5", EndpointKind::ChatCompletions),
            ("qwen3.8-max", EndpointKind::Messages),
            ("qwen3.5-plus", EndpointKind::Messages),
        ] {
            let qualified = format!("{MODEL_PREFIX}{id}");
            assert_eq!(
                resolve(id).expect("registered bare model").endpoint,
                endpoint
            );
            assert_eq!(
                resolve(&qualified)
                    .expect("registered provider-qualified model")
                    .endpoint,
                endpoint
            );
            assert!(advertised.iter().any(|model| model == id));
            assert!(advertised.contains(&qualified));
        }
    }

    #[test]
    fn canonical_prefix_resolves_and_unknown_models_do_not() {
        let spec = resolve("opencode-go/minimax-m3").expect("known model");
        assert_eq!(spec.id, "minimax-m3");
        assert_eq!(spec.endpoint, EndpointKind::Messages);
        assert_eq!(
            resolve("qwen3.7-plus").unwrap().endpoint,
            EndpointKind::Messages
        );
        assert!(resolve("opencode-go/not-a-model").is_none());
    }

    #[test]
    fn conflicting_provider_ids_are_only_advertised_with_prefix() {
        let models = advertised_models();
        for id in ["gpt-5.6-luna", "grok-4.5", "kimi-k3", "kimi-k2.6"] {
            assert!(!models.iter().any(|model| model == id));
            assert!(
                models
                    .iter()
                    .any(|model| model == &format!("opencode-go/{id}"))
            );
        }
    }
}
