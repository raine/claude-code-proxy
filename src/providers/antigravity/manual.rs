//! Private, explicit manual OAuth completion; never credential import.
use super::replay::Result;

#[cfg(unix)]
#[path = "manual_terminal.rs"]
pub(super) mod terminal;

pub(crate) const REDIRECT: &str = "https://antigravity.google/oauth-callback";
pub(crate) const INPUT_LIMIT: usize = 8192;

pub(crate) const INVALID: &str =
    "Invalid OAuth completion; paste the displayed code or a complete matching redirect URL";

fn valid_code(input: &str) -> bool {
    !input.is_empty()
        && input.len() <= INPUT_LIMIT
        && !input.starts_with("code=")
        && !input.starts_with("//")
        && !input.starts_with("/oauth-callback")
        && input
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !b":?#&%{}\"'\\<>".contains(&b))
}

pub(crate) fn parse_completion(input: &str, state: &str) -> Result<String> {
    if input.len() > INPUT_LIMIT || input.is_empty() {
        return Err(INVALID);
    }
    // A colon is never a bare-code byte: malformed URL-like input cannot fall
    // through to the PKCE-only path. Bare codes are opaque, not percent-decoded.
    if !input.contains(':') {
        return valid_code(input).then(|| input.to_owned()).ok_or(INVALID);
    }
    if !input.bytes().all(|b| (0x21..=0x7e).contains(&b)) || input.contains(['\\', '#']) {
        return Err(INVALID);
    }
    let (base, query) = input.split_once('?').ok_or(INVALID)?;
    let (scheme, rest) = base.split_once("://").ok_or(INVALID)?;
    let (authority, path) = rest.split_once('/').ok_or(INVALID)?;
    // Validate the raw target before Url can normalize backslashes/dot segments.
    if !scheme.eq_ignore_ascii_case("https")
        || !(authority.eq_ignore_ascii_case("antigravity.google")
            || authority.eq_ignore_ascii_case("antigravity.google:443"))
        || path != "oauth-callback"
    {
        return Err(INVALID);
    }
    let bytes = query.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && !(bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit))
        {
            return Err(INVALID);
        }
    }
    let url = url::Url::parse(input).map_err(|_| INVALID)?;
    let mut returned_state = None;
    let mut code = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "state" => &mut returned_state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if slot.is_some()
            || value.is_empty()
            || value.len() > INPUT_LIMIT
            || !value.bytes().all(|b| (0x21..=0x7e).contains(&b))
        {
            return Err(INVALID);
        }
        *slot = Some(value.into_owned());
    }
    if returned_state.as_deref() != Some(state) {
        return Err(INVALID);
    }
    match (code, error) {
        (Some(code), None) if valid_code(&code) => Ok(code),
        (None, Some(_)) => Err("OAuth consent denied"),
        _ => Err(INVALID),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_manual_bare_code_preserves_opaque_bytes() {
        for code in [
            "4/0Ab-_x",
            "code.with+plus~and=padding",
            "A",
            "xcode=inside",
        ] {
            assert_eq!(parse_completion(code, "state").unwrap(), code);
        }
        let maximum = "x".repeat(INPUT_LIMIT);
        assert_eq!(parse_completion(&maximum, "state").unwrap(), maximum);
    }

    #[test]
    fn antigravity_manual_url_validates_state_and_decodes_once() {
        for base in [REDIRECT, "HTTPS://ANTIGRAVITY.GOOGLE:443/oauth-callback"] {
            let url = format!("{base}?state=state&code=4%2F0Ab-_x%2Bok%3D&scope=benign");
            assert_eq!(parse_completion(&url, "state").unwrap(), "4/0Ab-_x+ok=");
        }
        assert_eq!(
            parse_completion(
                &format!(
                    "{REDIRECT}?state=state&error=access_denied&error_description=sentinel-private"
                ),
                "state"
            )
            .unwrap_err(),
            "OAuth consent denied"
        );
    }

    #[test]
    fn antigravity_manual_rejects_untrusted_or_ambiguous_completion() {
        let mut invalid = vec![
            "".to_string(),
            " ".into(),
            " x".into(),
            "x ".into(),
            "x\ny".into(),
            "é".into(),
            "x\u{7f}".into(),
            "x\0y".into(),
            "code=secret".into(),
            "//example/code".into(),
            "/oauth-callback?code=secret".into(),
            "https:/antigravity.google/oauth-callback".into(),
            "HTTPS:/antigravity.google/oauth-callback".into(),
            "https:antigravity.google/oauth-callback".into(),
            "other://secret".into(),
            "{\"code\":\"secret\"}".into(),
            "x%2Fy".into(),
            "x&state=y".into(),
            "x".repeat(INPUT_LIMIT + 1),
        ];
        for character in [':', '?', '#', '&', '%', '{', '}', '"', '\'', '\\', '<', '>'] {
            invalid.push(format!("a{character}b"));
        }
        for base in [
            "http://antigravity.google/oauth-callback",
            "https://evil.example/oauth-callback",
            "https://antigravity.google.evil.example/oauth-callback",
            "https://antigravity.google:444/oauth-callback",
            "https://user@antigravity.google/oauth-callback",
            "https://antigravity.google/other",
            "https://antigravity.google/oauth-callback/",
            "https://antigravity.google/x/../oauth-callback",
            "https://antigravity.google/%2e/oauth-callback",
            "https://antigravity.google\\oauth-callback",
        ] {
            invalid.push(format!("{base}?state=state&code=secret"));
        }
        for query in [
            "code=secret",
            "state=wrong&code=secret",
            "state=&code=secret",
            "state=state&state=state&code=secret",
            "state=state&%73tate=state&code=secret",
            "state=state&code=one&code=two",
            "state=state&code=",
            "state=state",
            "state=state&code=secret&error=denied",
            "state=state&error=one&error=two",
            "state=state&error=",
            "state=wrong&error=denied",
            "state=state&code=sec%ret",
            "state=state&code=secret%",
            "state=state&code=secret%2",
            "state=state&code=sec+ret",
            "state=state&code=sec%20ret",
            "state=state&code=%FF",
            "state=state&code=%00secret",
            "state=state&code=secret&extra=%GG",
            "state=state&code=secret#fragment",
            "state=state&code=%252F",
        ] {
            invalid.push(format!("{REDIRECT}?{query}"));
        }
        for input in invalid {
            let error =
                parse_completion(&input, "state").expect_err("untrusted completion accepted");
            assert!(!error.contains("secret"));
            assert!(!error.contains("sentinel"));
        }
    }
}
