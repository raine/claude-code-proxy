/// Maps the local name of a Grok model to the ID the backend serves it
/// under. The backend answers `grok-4.7-fast` with 404 and serves Grok 4.7
/// Fast only as `grok-4.7-build-fast`.
pub fn resolve_model(model: &str) -> String {
    match model {
        "grok-4.7-fast" => "grok-4.7-build-fast".to_string(),
        _ => model.to_string(),
    }
}

/// Checks a resolved backend ID, so local aliases are not listed here.
pub fn assert_allowed_model(model: &str) -> anyhow::Result<()> {
    if matches!(
        model,
        "grok-composer-2.5-fast" | "grok-4.5" | "grok-4.6" | "grok-4.7" | "grok-4.7-build-fast"
    ) {
        Ok(())
    } else {
        anyhow::bail!("unsupported Grok model")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_4_7_is_allowed() {
        assert!(assert_allowed_model("grok-4.7").is_ok());
    }

    #[test]
    fn fast_alias_resolves_to_backend_id_and_other_ids_pass_through() {
        let cases = [
            ("grok-4.7-fast", "grok-4.7-build-fast"),
            ("grok-4.7-build-fast", "grok-4.7-build-fast"),
            ("grok-4.7", "grok-4.7"),
            ("grok-4.6", "grok-4.6"),
            ("grok-composer-2.5-fast", "grok-composer-2.5-fast"),
        ];
        for (requested, expected) in cases {
            let resolved = resolve_model(requested);
            assert_eq!(resolved, expected, "{requested}");
            assert!(assert_allowed_model(&resolved).is_ok(), "{requested}");
        }
    }

    #[test]
    fn unserved_fast_and_build_ids_are_rejected() {
        for requested in ["grok-4.6-fast", "grok-4.7-build", "grok-4.5-fast"] {
            assert!(
                assert_allowed_model(&resolve_model(requested)).is_err(),
                "{requested}"
            );
        }
    }

    #[test]
    fn every_registered_grok_model_is_allowed_after_resolution() {
        for model in crate::registry::GROK_MODELS {
            assert!(
                assert_allowed_model(&resolve_model(model)).is_ok(),
                "{model}"
            );
        }
    }
}
