use http::HeaderMap;

pub const CLAUDE_SESSION_HEADER: &str = "x-claude-code-session-id";
pub const CLAUDE_AGENT_HEADER: &str = "x-claude-code-agent-id";
pub const CLAUDE_PARENT_AGENT_HEADER: &str = "x-claude-code-parent-agent-id";

const MAX_IDENTITY_LEN: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationIdentity {
    Main(String),
    Agent(String, String),
}

impl ConversationIdentity {
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let session = read_identity_header(headers, CLAUDE_SESSION_HEADER);
        let agent = read_identity_header(headers, CLAUDE_AGENT_HEADER);
        let parent = read_identity_header(headers, CLAUDE_PARENT_AGENT_HEADER);

        if session.is_invalid() || agent.is_invalid() || parent.is_invalid() {
            return None;
        }

        match (session.value(), agent.value(), parent.value()) {
            (Some(session_id), Some(agent_id), _) => {
                Some(Self::Agent(session_id.to_string(), agent_id.to_string()))
            }
            (Some(session_id), None, None) => Some(Self::Main(session_id.to_string())),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum ParsedHeader<'a> {
    Missing,
    Valid(&'a str),
    Invalid,
}

impl ParsedHeader<'_> {
    fn value(&self) -> Option<&str> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Missing | Self::Invalid => None,
        }
    }

    fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }
}

fn read_identity_header<'a>(headers: &'a HeaderMap, name: &str) -> ParsedHeader<'a> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return ParsedHeader::Missing;
    };
    if values.next().is_some() {
        return ParsedHeader::Invalid;
    }

    let Ok(value) = value.to_str() else {
        return ParsedHeader::Invalid;
    };
    let value = value.trim_matches(|character| matches!(character, ' ' | '\t'));
    if value.is_empty()
        || value.len() > MAX_IDENTITY_LEN
        || value.contains(',')
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return ParsedHeader::Invalid;
    }

    ParsedHeader::Valid(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn parses_main_agent_and_lineage_shapes() {
        let cases = [
            (
                "main",
                vec![(CLAUDE_SESSION_HEADER, "session-a")],
                Some(ConversationIdentity::Main("session-a".to_string())),
            ),
            (
                "direct agent without parent",
                vec![
                    (CLAUDE_SESSION_HEADER, "session-a"),
                    (CLAUDE_AGENT_HEADER, "agent-a"),
                ],
                Some(ConversationIdentity::Agent(
                    "session-a".to_string(),
                    "agent-a".to_string(),
                )),
            ),
            (
                "nested direct child",
                vec![
                    (CLAUDE_SESSION_HEADER, "session-a"),
                    (CLAUDE_AGENT_HEADER, "agent-child"),
                    (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
                ],
                Some(ConversationIdentity::Agent(
                    "session-a".to_string(),
                    "agent-child".to_string(),
                )),
            ),
            (
                "sibling one",
                vec![
                    (CLAUDE_SESSION_HEADER, "session-a"),
                    (CLAUDE_AGENT_HEADER, "agent-sibling-one"),
                    (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
                ],
                Some(ConversationIdentity::Agent(
                    "session-a".to_string(),
                    "agent-sibling-one".to_string(),
                )),
            ),
            (
                "sibling two",
                vec![
                    (CLAUDE_SESSION_HEADER, "session-a"),
                    (CLAUDE_AGENT_HEADER, "agent-sibling-two"),
                    (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
                ],
                Some(ConversationIdentity::Agent(
                    "session-a".to_string(),
                    "agent-sibling-two".to_string(),
                )),
            ),
            (
                "same agent in another session",
                vec![
                    (CLAUDE_SESSION_HEADER, "session-b"),
                    (CLAUDE_AGENT_HEADER, "agent-a"),
                ],
                Some(ConversationIdentity::Agent(
                    "session-b".to_string(),
                    "agent-a".to_string(),
                )),
            ),
            (
                "outer space and tab",
                vec![
                    (CLAUDE_SESSION_HEADER, " \tsession-a\t "),
                    (CLAUDE_AGENT_HEADER, "\tagent-a "),
                    (CLAUDE_PARENT_AGENT_HEADER, " agent-parent\t"),
                ],
                Some(ConversationIdentity::Agent(
                    "session-a".to_string(),
                    "agent-a".to_string(),
                )),
            ),
        ];

        for (name, values, expected) in cases {
            assert_eq!(
                ConversationIdentity::from_headers(&headers(&values)),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn parent_is_validation_only_and_never_changes_the_owner() {
        let direct = ConversationIdentity::from_headers(&headers(&[
            (CLAUDE_SESSION_HEADER, "session-a"),
            (CLAUDE_AGENT_HEADER, "agent-child"),
        ]));
        let nested = ConversationIdentity::from_headers(&headers(&[
            (CLAUDE_SESSION_HEADER, "session-a"),
            (CLAUDE_AGENT_HEADER, "agent-child"),
            (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
        ]));
        let reparented = ConversationIdentity::from_headers(&headers(&[
            (CLAUDE_SESSION_HEADER, "session-a"),
            (CLAUDE_AGENT_HEADER, "agent-child"),
            (CLAUDE_PARENT_AGENT_HEADER, "another-parent"),
        ]));

        assert_eq!(direct, nested);
        assert_eq!(nested, reparented);
    }

    #[test]
    fn ambiguous_or_absent_tuples_are_stateless() {
        let cases = [
            ("all missing", vec![]),
            (
                "agent without session",
                vec![(CLAUDE_AGENT_HEADER, "agent-a")],
            ),
            (
                "parent without direct agent",
                vec![
                    (CLAUDE_SESSION_HEADER, "session-a"),
                    (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
                ],
            ),
            (
                "parent alone",
                vec![(CLAUDE_PARENT_AGENT_HEADER, "agent-parent")],
            ),
            (
                "agent and parent without session",
                vec![
                    (CLAUDE_AGENT_HEADER, "agent-a"),
                    (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
                ],
            ),
        ];

        for (name, values) in cases {
            assert_eq!(
                ConversationIdentity::from_headers(&headers(&values)),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn rejects_malformed_text_in_every_identity_field() {
        let malformed = [
            ("empty", ""),
            ("spaces only", "   "),
            ("tabs only", "\t\t"),
            ("internal space", "two values"),
            ("internal tab", "two\tvalues"),
            ("leading comma", ",value"),
            ("trailing comma", "value,"),
            ("coalesced", "first, second"),
            ("oversize", "oversize-placeholder"),
        ];

        for field in [
            CLAUDE_SESSION_HEADER,
            CLAUDE_AGENT_HEADER,
            CLAUDE_PARENT_AGENT_HEADER,
        ] {
            for (shape, placeholder) in malformed {
                let value = if shape == "oversize" {
                    "x".repeat(MAX_IDENTITY_LEN + 1)
                } else {
                    placeholder.to_string()
                };
                let mut values = vec![
                    (CLAUDE_SESSION_HEADER, "session-a"),
                    (CLAUDE_AGENT_HEADER, "agent-a"),
                    (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
                ];
                values
                    .iter_mut()
                    .find(|(name, _)| *name == field)
                    .unwrap()
                    .1 = &value;
                assert_eq!(
                    ConversationIdentity::from_headers(&headers(&values)),
                    None,
                    "field={field} shape={shape}"
                );
            }
        }
    }

    #[test]
    fn rejects_duplicate_headers_in_every_identity_field() {
        for field in [
            CLAUDE_SESSION_HEADER,
            CLAUDE_AGENT_HEADER,
            CLAUDE_PARENT_AGENT_HEADER,
        ] {
            let mut values = vec![
                (CLAUDE_SESSION_HEADER, "session-a"),
                (CLAUDE_AGENT_HEADER, "agent-a"),
                (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
            ];
            values.push((field, "duplicate"));
            assert_eq!(
                ConversationIdentity::from_headers(&headers(&values)),
                None,
                "field={field}"
            );
        }
    }

    #[test]
    fn rejects_nontext_headers_in_every_identity_field() {
        for field in [
            CLAUDE_SESSION_HEADER,
            CLAUDE_AGENT_HEADER,
            CLAUDE_PARENT_AGENT_HEADER,
        ] {
            let mut values = headers(&[
                (CLAUDE_SESSION_HEADER, "session-a"),
                (CLAUDE_AGENT_HEADER, "agent-a"),
                (CLAUDE_PARENT_AGENT_HEADER, "agent-parent"),
            ]);
            values.insert(field, HeaderValue::from_bytes(&[0x80]).unwrap());
            assert_eq!(
                ConversationIdentity::from_headers(&values),
                None,
                "field={field}"
            );
        }
    }

    #[test]
    fn malformed_agent_cannot_downgrade_a_valid_session_to_main() {
        for malformed_agent in ["", "agent one", "agent-a,agent-b"] {
            let identity = ConversationIdentity::from_headers(&headers(&[
                (CLAUDE_SESSION_HEADER, "session-a"),
                (CLAUDE_AGENT_HEADER, malformed_agent),
            ]));
            assert_eq!(identity, None, "agent={malformed_agent:?}");
        }
    }
}
