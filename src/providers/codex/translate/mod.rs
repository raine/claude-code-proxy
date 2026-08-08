pub mod accumulate;
pub mod live_stream;
pub mod model_allowlist;
pub mod read_rewrite;
pub mod reasoning_signature;
pub mod reducer;
pub mod request;
pub mod stream;
pub mod web_search_compat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IncompleteResponsePolicy {
    Error,
    AllowMaxOutputTokens,
}
