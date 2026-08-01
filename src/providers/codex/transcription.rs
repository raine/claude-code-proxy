use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::StatusCode;
use serde::Deserialize;

pub const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_TRANSCRIPTION_REQUEST_BYTES: usize = MAX_AUDIO_BYTES + 1024 * 1024;
const MAX_TRANSCRIPTION_RESPONSE_BYTES: usize = 1024 * 1024;
const TRANSCRIPTION_BASE_URL: &str = "https://chatgpt.com/backend-api";

#[derive(Debug)]
pub struct TranscriptionRequestError {
    pub status: StatusCode,
    pub message: String,
    pub param: Option<&'static str>,
    pub code: &'static str,
}

impl TranscriptionRequestError {
    pub fn invalid(message: impl Into<String>, param: Option<&'static str>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            param,
            code: "invalid_request",
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            param: None,
            code: "invalid_upstream_response",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreparedTranscription {
    pub audio: Bytes,
    pub filename: String,
    pub content_type: String,
    pub language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TranscriptionResponse<'a> {
    #[serde(borrow)]
    text: &'a str,
}

pub struct CodexTranscriptionBackend {
    client: std::sync::Arc<super::client::CodexHttpClient>,
    base_url: String,
    limiter: std::sync::Arc<tokio::sync::Semaphore>,
}

impl CodexTranscriptionBackend {
    pub fn new() -> Self {
        Self {
            client: std::sync::Arc::new(super::client::CodexHttpClient::new()),
            base_url: TRANSCRIPTION_BASE_URL.to_string(),
            limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }

    #[cfg(test)]
    fn new_for_test(
        client: std::sync::Arc<super::client::CodexHttpClient>,
        base_url: String,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }

    pub async fn handle(
        &self,
        input: PreparedTranscription,
        ctx: crate::provider::RequestContext,
    ) -> Response {
        let _permit = match self.limiter.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return transcription_error_response(TranscriptionRequestError {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    message: "Too many concurrent transcription requests".to_string(),
                    param: None,
                    code: "local_capacity_exceeded",
                });
            }
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match self
            .client
            .post_transcription(&self.base_url, &input, &ctx)
            .await
        {
            Ok(response) => response,
            Err(error) => return transcription_transport_error_response(error),
        };
        let status = upstream.status();
        let headers = upstream.headers().clone();
        if status.is_redirection() {
            return transcription_error_response(TranscriptionRequestError::upstream(
                "Codex transcription service returned an unexpected redirect",
            ));
        }
        if !status.is_success() {
            let mut response = transcription_error_response(TranscriptionRequestError {
                status,
                message: format!(
                    "Codex transcription service returned HTTP {}",
                    status.as_u16()
                ),
                param: None,
                code: "upstream_error",
            });
            copy_safe_headers(&headers, response.headers_mut());
            return response;
        }
        let body = match collect_response_body(upstream, self.client.body_idle_timeout_ms()).await {
            Ok(body) => body,
            Err(error) => return transcription_error_response(error),
        };
        if let Err(error) = validate_success_response(&body) {
            return transcription_error_response(error);
        }
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.generation_started(&ctx.req_id);
        }
        let mut response = (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-store"),
        );
        response.headers_mut().insert(
            http::header::X_CONTENT_TYPE_OPTIONS,
            http::HeaderValue::from_static("nosniff"),
        );
        copy_safe_headers(&headers, response.headers_mut());
        response
    }
}

impl Default for CodexTranscriptionBackend {
    fn default() -> Self {
        Self::new()
    }
}

pub fn prepare_transcription(
    audio: Option<Bytes>,
    filename: Option<String>,
    content_type: Option<String>,
    language: Option<String>,
) -> Result<PreparedTranscription, TranscriptionRequestError> {
    let audio = audio.ok_or_else(|| {
        TranscriptionRequestError::invalid("Missing required 'file' field", Some("file"))
    })?;
    if audio.is_empty() {
        return Err(TranscriptionRequestError::invalid(
            "Uploaded audio file is empty",
            Some("file"),
        ));
    }
    if audio.len() > MAX_AUDIO_BYTES {
        return Err(TranscriptionRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Audio file must be at most {MAX_AUDIO_BYTES} bytes"),
            param: Some("file"),
            code: "request_too_large",
        });
    }
    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
    if !supported_audio_content_type(&content_type) {
        return Err(TranscriptionRequestError::invalid(
            format!("Unsupported audio content type '{content_type}'"),
            Some("file"),
        ));
    }
    let language = language
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if language.as_ref().is_some_and(|value| value.len() > 64) {
        return Err(TranscriptionRequestError::invalid(
            "'language' must be at most 64 characters",
            Some("language"),
        ));
    }
    Ok(PreparedTranscription {
        audio,
        filename: safe_filename(filename.as_deref().unwrap_or("audio.webm")),
        content_type,
        language,
    })
}

fn safe_filename(filename: &str) -> String {
    std::path::Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.replace(['\r', '\n', '"'], ""))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "audio.webm".to_string())
}

fn supported_audio_content_type(content_type: &str) -> bool {
    matches!(
        content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "audio/flac"
            | "audio/m4a"
            | "audio/mp3"
            | "audio/mp4"
            | "audio/mpeg"
            | "audio/ogg"
            | "audio/wav"
            | "audio/wave"
            | "audio/webm"
            | "audio/x-m4a"
            | "audio/x-wav"
    )
}

async fn collect_response_body(
    mut response: reqwest::Response,
    idle_timeout_ms: u64,
) -> Result<Vec<u8>, TranscriptionRequestError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TRANSCRIPTION_RESPONSE_BYTES as u64)
    {
        return Err(TranscriptionRequestError::upstream(
            "Codex transcription response exceeded the size limit",
        ));
    }
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout(
            std::time::Duration::from_millis(idle_timeout_ms),
            response.chunk(),
        )
        .await
        .map_err(|_| {
            TranscriptionRequestError::upstream("Timed out reading Codex transcription response")
        })?
        .map_err(|_| {
            TranscriptionRequestError::upstream("Failed to read Codex transcription response")
        })?;
        let Some(chunk) = chunk else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > MAX_TRANSCRIPTION_RESPONSE_BYTES {
            return Err(TranscriptionRequestError::upstream(
                "Codex transcription response exceeded the size limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_success_response(body: &[u8]) -> Result<(), TranscriptionRequestError> {
    let response: TranscriptionResponse<'_> = serde_json::from_slice(body).map_err(|_| {
        TranscriptionRequestError::upstream("Codex transcription service returned invalid JSON")
    })?;
    if response.text.trim().is_empty() {
        return Err(TranscriptionRequestError::upstream(
            "Codex transcription service returned no text",
        ));
    }
    Ok(())
}

fn copy_safe_headers(source: &http::HeaderMap, target: &mut http::HeaderMap) {
    for name in ["retry-after", "x-request-id", "openai-processing-ms"] {
        if let Some(value) = source.get(name) {
            target.insert(http::HeaderName::from_static(name), value.clone());
        }
    }
}

fn transcription_transport_error_response(error: super::client::CodexError) -> Response {
    let status = match error.status {
        401 => StatusCode::UNAUTHORIZED,
        403 => StatusCode::FORBIDDEN,
        429 => StatusCode::TOO_MANY_REQUESTS,
        value if (400..=599).contains(&value) => {
            StatusCode::from_u16(value).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        _ => StatusCode::BAD_GATEWAY,
    };
    let mut response = transcription_error_response(TranscriptionRequestError {
        status,
        message: if error.status == 0 {
            "Codex transcription service is unavailable".to_string()
        } else {
            format!("Codex transcription service returned HTTP {}", error.status)
        },
        param: None,
        code: if status == StatusCode::UNAUTHORIZED {
            "authentication_error"
        } else if status == StatusCode::FORBIDDEN {
            "permission_error"
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limit_error"
        } else {
            "upstream_error"
        },
    });
    if let Some(retry_after) = error.retry_after
        && let Ok(value) = http::HeaderValue::from_str(&retry_after)
    {
        response
            .headers_mut()
            .insert(http::header::RETRY_AFTER, value);
    }
    response
}

pub fn transcription_error_response(error: TranscriptionRequestError) -> Response {
    let error_type = match error.status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        status if status.is_client_error() => "invalid_request_error",
        _ => "api_error",
    };
    (
        error.status,
        [
            (http::header::CONTENT_TYPE, "application/json"),
            (http::header::CACHE_CONTROL, "no-store"),
        ],
        axum::Json(serde_json::json!({
            "error": {
                "message": error.message,
                "type": error_type,
                "param": error.param,
                "code": error.code,
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> crate::provider::RequestContext {
        crate::provider::RequestContext {
            req_id: "transcription-test".to_string(),
            session_id: None,
            session_seq: None,
            provider: "codex".to_string(),
            traffic: None,
            monitor: None,
        }
    }

    #[test]
    fn validates_audio_and_normalizes_filename() {
        let prepared = prepare_transcription(
            Some(Bytes::from_static(b"audio")),
            Some("../recording.webm".to_string()),
            Some("audio/webm;codecs=opus".to_string()),
            Some(" en ".to_string()),
        )
        .unwrap();
        assert_eq!(prepared.filename, "recording.webm");
        assert_eq!(prepared.language.as_deref(), Some("en"));

        assert!(prepare_transcription(None, None, None, None).is_err());
        assert!(
            prepare_transcription(
                Some(Bytes::from_static(b"audio")),
                None,
                Some("text/plain".to_string()),
                None,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn backend_forwards_multipart_with_codex_auth() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap();
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request);
            assert!(request_text.starts_with("POST /root/transcribe HTTP/1.1"));
            assert!(request_text.contains("authorization: Bearer test"));
            assert!(request_text.contains("chatgpt-account-id: acct"));
            assert!(request_text.contains("originator: Codex Desktop"));
            assert!(request_text.contains("name=\"file\""));
            assert!(request_text.contains("filename=\"recording.webm\""));
            assert!(request_text.contains("name=\"language\""));
            assert!(request_text.contains("\r\nen\r\n"));
            assert!(request_text.contains("audio-bytes"));

            let body = br#"{"text":"hello world"}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-request-id: upstream-transcribe\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });

        let client = super::super::client::CodexHttpClient::new_for_test(
            reqwest::Client::builder().no_proxy().build().unwrap(),
            format!("http://{addr}/responses"),
            1_000,
            1_000,
            0,
        );
        client
            .auth_manager()
            .set_test_auth(super::super::auth::token_store::StoredAuth {
                access: "test".into(),
                refresh: String::new(),
                account_id: Some("acct".into()),
                expires: u64::MAX,
            });
        let backend = CodexTranscriptionBackend::new_for_test(
            std::sync::Arc::new(client),
            format!("http://{addr}/root"),
        );
        let response = backend
            .handle(
                prepare_transcription(
                    Some(Bytes::from_static(b"audio-bytes")),
                    Some("recording.webm".to_string()),
                    Some("audio/webm".to_string()),
                    Some("en".to_string()),
                )
                .unwrap(),
                context(),
            )
            .await;
        server.await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-request-id"], "upstream-transcribe");
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body, br#"{"text":"hello world"}"#.as_slice());
    }

    #[test]
    fn validates_success_payload() {
        assert!(validate_success_response(br#"{"text":"hello"}"#).is_ok());
        assert!(validate_success_response(br#"{"text":""}"#).is_err());
        assert!(validate_success_response(br#"{}"#).is_err());
    }
}
