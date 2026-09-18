pub mod error;
pub mod schema;
pub mod sse;

pub const MAX_ANTHROPIC_REQUEST_BYTES: usize = 64 * 1024 * 1024;

pub use self::error::{ErrorDetail, ErrorEnvelope, json_error};
pub use self::schema::{CountTokensResponse, Message, MessagesRequest};
pub use self::sse::{
    SseEvent, SseParseStats, encode_sse_event, parse_sse_events, parse_sse_events_with_stats,
};
