use prost::Message;

use crate::config;
use crate::providers::cursor::connect::{
    ConnectFrame, ConnectFrameDecoder, FLAG_END, FLAG_GZIP, encode_connect_frame,
    parse_connect_error,
};
use crate::providers::cursor::model::CursorModelResolution;
use crate::providers::cursor::proto::{self, AgentClientMessage, RunRequest};
use crate::providers::cursor::request::CursorSelectedImage;

/// Upstream response from the Cursor API.
///
/// Contains the raw response bytes (or body bytes for streaming) and the
/// HTTP status.
pub struct CursorUpstreamResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub error_detail: Option<String>,
}

impl CursorUpstreamResponse {
    pub fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// HTTP/2 client for the Cursor AgentService/Run endpoint.
pub struct CursorHttpClient {
    client: reqwest::Client,
    base_url: String,
}

impl Default for CursorHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorHttpClient {
    pub fn new() -> Self {
        // Use HTTP/2 prior knowledge for cleartext URLs (mock testing) and
        // standard TLS for https URLs.
        let base_url = config::cursor_base_url();
        let is_cleartext = base_url.starts_with("http://");

        let mut builder = reqwest::Client::builder()
            .http2_keep_alive_timeout(std::time::Duration::from_secs(30))
            .http2_keep_alive_while_idle(true);

        if is_cleartext {
            builder = builder.http2_prior_knowledge();
        }

        let client = builder.build().expect("CursorHttpClient: reqwest client");

        Self { client, base_url }
    }

    /// Run the Cursor agent with the given prompt and token.
    ///
    /// Builds the prost RunRequest, encodes it in a Connect frame, and
    /// sends it via HTTP/2 POST.
    pub async fn run_agent(
        &self,
        token: &str,
        prompt: &str,
        model: &str,
        images: &[CursorSelectedImage],
    ) -> Result<CursorUpstreamResponse, CursorError> {
        let resolved = super::model::resolve_cursor_model(model)
            .map_err(|e| CursorError::internal(format!("model resolution: {e}")))?;

        let request_id = uuid::Uuid::new_v4().to_string();

        // Build the prost RunRequest
        let run_request = build_run_request(prompt, &resolved, images, &request_id);

        let msg = AgentClientMessage {
            run_request: Some(run_request),
            client_heartbeat: None,
        };

        let mut payload = Vec::new();
        msg.encode(&mut payload)
            .map_err(|e| CursorError::internal(format!("prost encode: {e}")))?;

        let body = encode_connect_frame(&payload, 0);

        let url = format!(
            "{}/agent.v1.AgentService/Run",
            self.base_url.trim_end_matches('/')
        );

        let client_version = config::cursor_client_version();

        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip,br")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &client_version)
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .header("x-cursor-streaming", "true")
            .header("te", "trailers")
            .body(body)
            .send()
            .await
            .map_err(CursorError::from_reqwest)?;

        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let error_detail = resp
            .headers()
            .get("grpc-message")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| CursorError::internal(format!("read body: {e}")))?;

        if status >= 400 {
            // Try to extract Connect error from body
            let detail = parse_error_body(&body_bytes, &headers);
            return Err(CursorError::new(status, "Cursor upstream error", detail));
        }

        Ok(CursorUpstreamResponse {
            status,
            body: body_bytes.to_vec(),
            error_detail,
        })
    }
}

fn build_run_request(
    prompt: &str,
    resolved: &CursorModelResolution,
    images: &[CursorSelectedImage],
    request_id: &str,
) -> RunRequest {
    let selected_images: Vec<proto::SelectedImage> = images
        .iter()
        .map(|img| proto::SelectedImage {
            data: img.data.clone(),
            uuid: img.uuid.clone(),
            path: img.path.clone(),
            mime_type: img.mime_type.clone(),
        })
        .collect();

    RunRequest {
        conversation_state: Some(proto::ConversationState {
            messages: Vec::new(),
        }),
        action: Some(proto::Action {
            user_message_action: Some(proto::UserMessageAction {
                user_message: Some(proto::UserMessage {
                    text: prompt.to_string(),
                    message_id: request_id.to_string(),
                    selected_context: if selected_images.is_empty() {
                        None
                    } else {
                        Some(proto::SelectedContext { selected_images })
                    },
                    mode: resolved.mode.as_str().to_string(),
                }),
            }),
        }),
        mcp_tools: None,
        conversation_id: String::new(),
        requested_model: Some(proto::CursorModel {
            model_id: resolved.model_id.clone(),
            parameters: Vec::new(),
        }),
        exclude_workspace_context: false,
        selected_subagent_models: vec![],
        conversation_group_id: String::new(),
        client_supports_inline_images: true,
    }
}

fn parse_error_body(body_bytes: &[u8], _headers: &reqwest::header::HeaderMap) -> Option<String> {
    if body_bytes.len() < 5 {
        return None;
    }
    // Try to parse as Connect end frame with JSON error
    if body_bytes.len() >= 5 {
        let flags = body_bytes[0];
        let len = u32::from_be_bytes([body_bytes[1], body_bytes[2], body_bytes[3], body_bytes[4]])
            as usize;
        if flags & FLAG_END != 0 && body_bytes.len() >= 5 + len {
            let payload = &body_bytes[5..5 + len];
            let err = parse_connect_error(payload);
            if err.is_some() {
                return err.map(|e| e.detail);
            }
        }
    }

    // Try plain text error
    if let Ok(text) = String::from_utf8(body_bytes.to_vec())
        && !text.is_empty()
    {
        return Some(text);
    }
    None
}

/// Decode upstream response bytes into Connect frames containing
/// AgentServerMessage values.
pub fn decode_upstream_frames(body: &[u8]) -> Result<Vec<ConnectFrame>, CursorError> {
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder
        .push(body)
        .map_err(|e| CursorError::internal(format!("frame decode: {e}")))?;
    Ok(frames)
}

/// Decode a single Connect frame payload into an AgentServerMessage.
/// Handles gzip decompression if the FLAG_GZIP bit is set.
pub fn decode_frame_payload(
    frame: &ConnectFrame,
) -> Result<proto::AgentServerMessage, CursorError> {
    let payload = if frame.flags & FLAG_GZIP != 0 {
        super::connect::decode_gzip_frame(&frame.payload)
            .map_err(|e| CursorError::internal(format!("gzip decompress: {e}")))?
    } else {
        frame.payload.to_vec()
    };

    proto::AgentServerMessage::decode(&payload[..])
        .map_err(|e| CursorError::internal(format!("prost decode: {e}")))
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CursorError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
}

impl CursorError {
    pub fn new(status: u16, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail,
            retry_after: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            detail: None,
            retry_after: None,
        }
    }

    pub fn from_reqwest(e: reqwest::Error) -> Self {
        let status = e.status().map(|s| s.as_u16()).unwrap_or(502);
        Self {
            status,
            message: e.to_string(),
            detail: None,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for CursorError {}
