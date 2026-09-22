pub fn resolve_model(model: &str) -> String {
    model.to_string()
}

pub fn assert_allowed_model(model: &str) -> anyhow::Result<()> {
    if matches!(
        model,
        "grok-composer-2.5-fast" | "grok-4.5" | "grok-4.6" | "grok-4.7"
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
}
