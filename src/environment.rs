//! Legacy UTF-8 environment snapshots exclude settings validated independently as OS strings.

use std::ffi::{OsStr, OsString};

pub(crate) const ANTIGRAVITY_USER_AGENT: &str = "CCP_ANTIGRAVITY_USER_AGENT";

/// Keep the native header out of legacy snapshots so its own OsString-aware
/// validator can reject invalid input without affecting unrelated providers.
/// This does not mutate the process environment or relax other invalid entries.
pub(crate) fn legacy_vars() -> impl Iterator<Item = (String, String)> {
    legacy_utf8_entries(std::env::vars_os(), cfg!(windows))
}

fn is_native_user_agent(name: &OsStr, windows: bool) -> bool {
    name.to_str().is_some_and(|name| {
        if windows {
            name.eq_ignore_ascii_case(ANTIGRAVITY_USER_AGENT)
        } else {
            name == ANTIGRAVITY_USER_AGENT
        }
    })
}

fn legacy_utf8_entries(
    entries: impl IntoIterator<Item = (OsString, OsString)>,
    windows: bool,
) -> impl Iterator<Item = (String, String)> {
    entries
        .into_iter()
        .filter(move |(name, _)| !is_native_user_agent(name, windows))
        .map(|(name, value)| {
            // Unwrapping into_string() errors would print the invalid input.
            // Preserve failure semantics with a fixed, non-sensitive message.
            let name = name
                .into_string()
                .unwrap_or_else(|_| panic!("non-UTF-8 environment entry"));
            let value = value
                .into_string()
                .unwrap_or_else(|_| panic!("non-UTF-8 environment entry"));
            (name, value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn entries(values: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        values
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn native_user_agent_environment_iterator_preserves_unrelated_entries_and_order() {
        let input = entries(&[
            ("FIRST", ""),
            (ANTIGRAVITY_USER_AGENT, "native-UA"),
            ("環境", "данные"),
            (ANTIGRAVITY_USER_AGENT, ""),
            ("FIRST", "second value"),
            ("CCP_USER_AGENT", "legacy-UA"),
        ]);
        let output: Vec<_> = legacy_utf8_entries(input, false).collect();
        assert_eq!(
            output,
            vec![
                ("FIRST".to_owned(), "".to_owned()),
                ("環境".to_owned(), "данные".to_owned()),
                ("FIRST".to_owned(), "second value".to_owned()),
                ("CCP_USER_AGENT".to_owned(), "legacy-UA".to_owned()),
            ]
        );
    }

    #[test]
    fn native_user_agent_environment_key_matching_is_platform_scoped() {
        for windows in [false, true] {
            for key in [
                "CCP_ANTIGRAVITY_USER_AGENT",
                "ccp_antigravity_user_agent",
                "Ccp_Antigravity_User_Agent",
            ] {
                let output: Vec<_> =
                    legacy_utf8_entries(entries(&[(key, "value")]), windows).collect();
                assert_eq!(output.is_empty(), windows || key == ANTIGRAVITY_USER_AGENT);
            }
            for key in [
                "CCP_ANTIGRAVITY_USER_AGENT_EXTRA",
                "XCCP_ANTIGRAVITY_USER_AGENT",
                "CCP_ANTIGRAVITY_USER_AGENŤ",
                "",
                "OTHER",
            ] {
                let output: Vec<_> =
                    legacy_utf8_entries(entries(&[(key, "value")]), windows).collect();
                assert_eq!(output, vec![(key.to_owned(), "value".to_owned())]);
            }
        }
        // This checks the compiled platform selector; Windows execution remains a separate gate.
        let output: Vec<_> = legacy_utf8_entries(
            entries(&[("ccp_antigravity_user_agent", "value")]),
            cfg!(windows),
        )
        .collect();
        assert_eq!(output.is_empty(), cfg!(windows));
    }

    #[cfg(unix)]
    fn invalid_os_string() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(b"synthetic-invalid-\xff".to_vec())
    }

    #[cfg(windows)]
    fn invalid_os_string() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[b'x' as u16, 0xd800])
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn native_user_agent_environment_omission_does_not_hide_native_invalid_presence() {
        let input = vec![(OsString::from(ANTIGRAVITY_USER_AGENT), invalid_os_string())];
        let output: Vec<_> = legacy_utf8_entries(input.clone(), cfg!(windows)).collect();
        assert!(output.is_empty());
        let directory = tempfile::tempdir().unwrap();
        let result = crate::providers::antigravity::config::Config::read(
            &directory.path().join("config.json"),
            |key| {
                input
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            },
        );
        assert_eq!(result.err(), Some("invalid Antigravity user agent"));
        assert!(
            input[0].1.to_str().is_none(),
            "the original fixture must remain unmodified"
        );
        for key in [
            "CCP_ANTIGRAVITY_USER_AGENT",
            "ccp_antigravity_user_agent",
            "Ccp_Antigravity_User_Agent",
        ] {
            let output: Vec<_> =
                legacy_utf8_entries(vec![(key.into(), invalid_os_string())], true).collect();
            assert!(output.is_empty());
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn native_user_agent_environment_unrelated_invalid_entries_fail_without_raw_values() {
        for input in [
            vec![(invalid_os_string(), "value".into())],
            vec![("OTHER".into(), invalid_os_string())],
            vec![("ccp_antigravity_user_agent".into(), invalid_os_string())],
        ] {
            let panic =
                std::panic::catch_unwind(|| legacy_utf8_entries(input, false).collect::<Vec<_>>())
                    .unwrap_err();
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert_eq!(message, Some("non-UTF-8 environment entry"));
        }
    }
}
