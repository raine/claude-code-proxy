use bytes::Bytes; // EFFICIENCY: avoid memcpy on multipart upload
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const IMAGE_MODEL: &str = "gpt-image-2";
pub const MAX_GENERATION_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_EDIT_REQUEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IMAGE_RESPONSE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_EDIT_IMAGES: usize = 5;
pub const MAX_SINGLE_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_EDIT_IMAGE_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOperation {
    Generation,
    Edit,
}

impl ImageOperation {
    pub fn upstream_path(self) -> &'static str {
        match self {
            Self::Generation => "images/generations",
            Self::Edit => "images/edits",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Generation => "generation",
            Self::Edit => "edit",
        }
    }
}

#[derive(Debug)]
pub struct ImageRequestError {
    pub status: StatusCode,
    pub message: String,
    pub param: Option<&'static str>,
    pub code: Option<&'static str>,
}

impl ImageRequestError {
    fn invalid(message: impl Into<String>, param: Option<&'static str>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            param,
            code: Some("invalid_request"),
        }
    }

    fn upstream_invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            param: None,
            code: Some("invalid_upstream_response"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ImageResponse<'a> {
    created: u64,
    #[serde(borrow)]
    data: Vec<ImageResponseItem<'a>>,
    #[serde(default)]
    usage: Option<ImageUsage>,
}

#[derive(Debug, Deserialize)]
struct ImageResponseItem<'a> {
    #[serde(borrow)]
    b64_json: &'a str,
}

#[derive(Debug, Deserialize)]
struct ImageUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationRequest {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u8>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageUrl {
    image_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    prompt: String,
    images: Vec<ImageUrl>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u8>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Debug)]
pub struct UploadedImage {
    pub bytes: Bytes,
}

#[derive(Debug, Default)]
pub struct MultipartEditInput {
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub background: Option<String>,
    pub n: Option<u8>,
    pub quality: Option<String>,
    pub size: Option<String>,
    pub images: Vec<UploadedImage>,
}

#[derive(Debug)]
pub struct PreparedImageRequest {
    pub body: Value,
    pub model: String,
    pub image_count: usize,
}

pub struct CodexImagesBackend {
    client: std::sync::Arc<super::client::CodexHttpClient>,
    base_url: String,
    limiter: std::sync::Arc<tokio::sync::Semaphore>,
}

impl CodexImagesBackend {
    pub fn new() -> Result<Self, String> {
        let base_url = validate_image_base_url(&crate::config::codex_images_base_url())?;
        Ok(Self {
            client: std::sync::Arc::new(super::client::CodexHttpClient::new()),
            base_url,
            limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }

    #[cfg(test)]
    fn new_for_test(
        client: std::sync::Arc<super::client::CodexHttpClient>,
        base_url: String,
    ) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            limiter: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    pub async fn handle(
        &self,
        operation: ImageOperation,
        prepared: PreparedImageRequest,
        ctx: crate::provider::RequestContext,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;

        let _permit = match self.limiter.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return image_error_response(ImageRequestError {
                    status: StatusCode::TOO_MANY_REQUESTS,
                    message: "Too many concurrent image requests".to_string(),
                    param: None,
                    code: Some("local_capacity_exceeded"),
                });
            }
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.model_resolved(&ctx.req_id, &prepared.model);
            monitor.upstream_started(&ctx.req_id);
        }
        let upstream = match self
            .client
            .post_image_json(&self.base_url, operation, &prepared.body, &ctx)
            .await
        {
            Ok(response) => response,
            Err(error) => return image_transport_error_response(error),
        };
        let status = upstream.status();
        let headers = upstream.headers().clone();
        // EFFICIENCY: check status and content-length vs size budget before reading body
        if status.is_redirection() {
            return image_error_response(ImageRequestError::upstream_invalid(
                "Codex image service returned an unexpected redirect",
            ));
        }
        if upstream
            .content_length()
            .is_some_and(|length| length > MAX_IMAGE_RESPONSE_BYTES as u64)
        {
            return image_error_response(ImageRequestError::upstream_invalid(
                "Codex image response exceeded the size limit",
            ));
        }
        if !status.is_success() {
            // EFFICIENCY: for error responses, consume a small diagnostic prefix rather than full body
            let mut response = image_error_response(ImageRequestError {
                status,
                message: format!("Codex image service returned HTTP {}", status.as_u16()),
                param: None,
                code: Some("upstream_error"),
            });
            copy_safe_image_headers(&headers, response.headers_mut());
            return response;
        }
        let body =
            match collect_image_response_body(upstream, self.client.body_idle_timeout_ms(), &ctx)
                .await
            {
                Ok(body) => body,
                Err(error) => return image_error_response(error),
            };
        let usage = match validate_success_response(&body) {
            Ok(usage) => usage,
            Err(error) => return image_error_response(error),
        };
        if let Some(monitor) = ctx.monitor.as_ref() {
            monitor.usage_updated(&ctx.req_id, usage.0, usage.1);
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
        copy_safe_image_headers(&headers, response.headers_mut());
        response
    }
}

async fn collect_image_response_body(
    mut response: reqwest::Response,
    body_idle_timeout_ms: u64,
    ctx: &crate::provider::RequestContext,
) -> Result<Vec<u8>, ImageRequestError> {
    // EFFICIENCY: preallocate from Content-Length when available to avoid repeated reallocs
    let cap = response
        .content_length()
        .map(|l| l as usize)
        .unwrap_or(0)
        .min(MAX_IMAGE_RESPONSE_BYTES);
    let mut body = Vec::with_capacity(cap);
    let mut started = false;
    loop {
        let chunk = tokio::time::timeout(
            std::time::Duration::from_millis(body_idle_timeout_ms),
            response.chunk(),
        )
        .await
        .map_err(|_| ImageRequestError::upstream_invalid("Timed out reading Codex image response"))?
        .map_err(|_| ImageRequestError::upstream_invalid("Failed to read Codex image response"))?;
        let Some(chunk) = chunk else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > MAX_IMAGE_RESPONSE_BYTES {
            return Err(ImageRequestError::upstream_invalid(
                "Codex image response exceeded the size limit",
            ));
        }
        if !started {
            if let Some(monitor) = ctx.monitor.as_ref() {
                monitor.generation_started(&ctx.req_id);
            }
            started = true;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn copy_safe_image_headers(source: &http::HeaderMap, target: &mut http::HeaderMap) {
    for name in [
        "retry-after",
        "x-request-id",
        "openai-processing-ms",
        "openai-version",
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
    ] {
        if let Some(value) = source.get(name) {
            target.insert(http::HeaderName::from_static(name), value.clone());
        }
    }
}

fn image_transport_error_response(error: super::client::CodexError) -> axum::response::Response {
    let status = match error.status {
        401 => StatusCode::UNAUTHORIZED,
        403 => StatusCode::FORBIDDEN,
        429 => StatusCode::TOO_MANY_REQUESTS,
        value if (400..=599).contains(&value) => {
            StatusCode::from_u16(value).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        _ => StatusCode::BAD_GATEWAY,
    };
    let mut response = image_error_response(ImageRequestError {
        status,
        message: if error.status == 0 {
            "Codex image service is unavailable".to_string()
        } else {
            format!("Codex image service returned HTTP {}", error.status)
        },
        param: None,
        code: Some(if status == StatusCode::UNAUTHORIZED {
            "authentication_error"
        } else if status == StatusCode::FORBIDDEN {
            "permission_error"
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limit_error"
        } else {
            "upstream_error"
        }),
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

pub fn image_error_response(error: ImageRequestError) -> axum::response::Response {
    use axum::response::IntoResponse;

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

pub fn prepare_json_request(
    operation: ImageOperation,
    bytes: &[u8],
) -> Result<PreparedImageRequest, ImageRequestError> {
    match operation {
        ImageOperation::Generation => prepare_generation_request(bytes),
        ImageOperation::Edit => prepare_edit_request(bytes),
    }
}

fn prepare_generation_request(bytes: &[u8]) -> Result<PreparedImageRequest, ImageRequestError> {
    let mut request: GenerationRequest = serde_json::from_slice(bytes).map_err(|error| {
        ImageRequestError::invalid(format!("Invalid JSON image request: {error}"), None)
    })?;
    validate_and_default_common(
        &request.prompt,
        &mut request.model,
        &mut request.background,
        request.n,
        &mut request.quality,
        &mut request.size,
    )?;
    let model = request.model.clone().expect("model defaulted");
    let body = serde_json::to_value(request).map_err(|error| ImageRequestError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("Failed to serialize image request: {error}"),
        param: None,
        code: Some("internal_error"),
    })?;
    Ok(PreparedImageRequest {
        body,
        model,
        image_count: 0,
    })
}

pub fn prepare_multipart_edit(
    input: MultipartEditInput,
) -> Result<PreparedImageRequest, ImageRequestError> {
    use base64::Engine as _;

    if input.images.is_empty() || input.images.len() > MAX_EDIT_IMAGES {
        return Err(ImageRequestError::invalid(
            format!("'image' must contain between 1 and {MAX_EDIT_IMAGES} files"),
            Some("image"),
        ));
    }
    let total_bytes = input.images.iter().try_fold(0usize, |total, image| {
        if image.bytes.len() > MAX_SINGLE_IMAGE_BYTES {
            return Err(ImageRequestError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: format!("Each image must be at most {MAX_SINGLE_IMAGE_BYTES} bytes"),
                param: Some("image"),
                code: Some("request_too_large"),
            });
        }
        total
            .checked_add(image.bytes.len())
            .ok_or(ImageRequestError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: "Combined image payload is too large".to_string(),
                param: Some("image"),
                code: Some("request_too_large"),
            })
    })?;
    if total_bytes > MAX_EDIT_IMAGE_BYTES {
        return Err(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Combined images must be at most {MAX_EDIT_IMAGE_BYTES} bytes"),
            param: Some("image"),
            code: Some("request_too_large"),
        });
    }

    let images = input
        .images
        .into_iter()
        .map(|image| {
            let mime = detect_image_mime(&image.bytes).ok_or_else(|| {
                ImageRequestError::invalid("Unsupported or malformed image file", Some("image"))
            })?;
            // EFFICIENCY: preallocate data-URL prefix, then encode image bytes into the same buffer
            let mut data_url = format!("data:{mime};base64,");
            base64::engine::general_purpose::STANDARD.encode_string(&image.bytes, &mut data_url);
            Ok(ImageUrl {
                image_url: data_url,
            })
        })
        .collect::<Result<Vec<_>, ImageRequestError>>()?;
    let request = EditRequest {
        prompt: input.prompt.ok_or_else(|| {
            ImageRequestError::invalid("Missing required 'prompt' field", Some("prompt"))
        })?,
        images,
        model: input.model,
        background: input.background,
        n: input.n,
        quality: input.quality,
        size: input.size,
    };
    prepare_edit_value(request)
}

fn prepare_edit_request(bytes: &[u8]) -> Result<PreparedImageRequest, ImageRequestError> {
    let request: EditRequest = serde_json::from_slice(bytes).map_err(|error| {
        ImageRequestError::invalid(format!("Invalid JSON image edit request: {error}"), None)
    })?;
    prepare_edit_value(request)
}

fn prepare_edit_value(mut request: EditRequest) -> Result<PreparedImageRequest, ImageRequestError> {
    if request.images.is_empty() || request.images.len() > MAX_EDIT_IMAGES {
        return Err(ImageRequestError::invalid(
            format!("'images' must contain between 1 and {MAX_EDIT_IMAGES} items"),
            Some("images"),
        ));
    }
    let total_bytes = request.images.iter().try_fold(0usize, |total, image| {
        let image_bytes = validate_data_url(&image.image_url)?;
        total.checked_add(image_bytes).ok_or(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Combined image payload is too large".to_string(),
            param: Some("images"),
            code: Some("request_too_large"),
        })
    })?;
    if total_bytes > MAX_EDIT_IMAGE_BYTES {
        return Err(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Combined images must be at most {MAX_EDIT_IMAGE_BYTES} bytes"),
            param: Some("images"),
            code: Some("request_too_large"),
        });
    }
    validate_and_default_common(
        &request.prompt,
        &mut request.model,
        &mut request.background,
        request.n,
        &mut request.quality,
        &mut request.size,
    )?;
    let model = request.model.clone().expect("model defaulted");
    let image_count = request.images.len();
    let body = serde_json::to_value(request).map_err(|error| ImageRequestError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("Failed to serialize image edit request: {error}"),
        param: None,
        code: Some("internal_error"),
    })?;
    Ok(PreparedImageRequest {
        body,
        model,
        image_count,
    })
}

fn validate_data_url(value: &str) -> Result<usize, ImageRequestError> {
    use base64::Engine as _;

    let (metadata, encoded) = value.split_once(',').ok_or_else(|| {
        ImageRequestError::invalid("Image must be a base64 data URL", Some("images"))
    })?;
    let mime = metadata
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or_else(|| {
            ImageRequestError::invalid("Image must be a base64 data URL", Some("images"))
        })?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| {
            ImageRequestError::invalid("Image data is not valid base64", Some("images"))
        })?;
    let detected = detect_image_mime(&decoded).ok_or_else(|| {
        ImageRequestError::invalid("Unsupported or malformed image data", Some("images"))
    })?;
    if mime != detected {
        return Err(ImageRequestError::invalid(
            format!("Image media type '{mime}' does not match '{detected}' data"),
            Some("images"),
        ));
    }
    if decoded.len() > MAX_SINGLE_IMAGE_BYTES {
        return Err(ImageRequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("Each image must be at most {MAX_SINGLE_IMAGE_BYTES} bytes"),
            param: Some("images"),
            code: Some("request_too_large"),
        });
    }
    Ok(decoded.len())
}

pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn validate_success_response(
    bytes: &[u8],
) -> Result<(Option<u64>, Option<u64>), ImageRequestError> {
    let response: ImageResponse<'_> = serde_json::from_slice(bytes).map_err(|_| {
        ImageRequestError::upstream_invalid("Codex image service returned invalid JSON")
    })?;
    let _created = response.created;
    if response.data.is_empty() || response.data.iter().any(|item| item.b64_json.is_empty()) {
        return Err(ImageRequestError::upstream_invalid(
            "Codex image service returned no image data",
        ));
    }
    Ok(response
        .usage
        .map(|usage| (usage.input_tokens, usage.output_tokens))
        .unwrap_or((None, None)))
}

pub fn validate_image_base_url(raw: &str) -> Result<String, String> {
    let parsed =
        url::Url::parse(raw).map_err(|error| format!("Invalid Codex images base URL: {error}"))?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("chatgpt.com")
        || parsed.port_or_known_default() != Some(443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.path().starts_with("/backend-api/codex")
    {
        return Err(
            "Codex images base URL must be an HTTPS chatgpt.com/backend-api/codex URL without credentials, query, or fragment"
                .to_string(),
        );
    }
    Ok(raw.trim_end_matches('/').to_string())
}

fn validate_and_default_common(
    prompt: &str,
    model: &mut Option<String>,
    background: &mut Option<String>,
    n: Option<u8>,
    quality: &mut Option<String>,
    size: &mut Option<String>,
) -> Result<(), ImageRequestError> {
    if prompt.trim().is_empty() {
        return Err(ImageRequestError::invalid(
            "'prompt' must not be empty",
            Some("prompt"),
        ));
    }
    match model.as_deref() {
        Some(IMAGE_MODEL) | None => {}
        Some(other) => {
            return Err(ImageRequestError::invalid(
                format!("Unsupported image model '{other}'; expected '{IMAGE_MODEL}'"),
                Some("model"),
            ));
        }
    }
    if n.is_some_and(|n| !(1..=10).contains(&n)) {
        return Err(ImageRequestError::invalid(
            "'n' must be between 1 and 10",
            Some("n"),
        ));
    }
    validate_choice(
        "background",
        background.as_deref(),
        &["auto", "transparent", "opaque"],
    )?;
    validate_choice(
        "quality",
        quality.as_deref(),
        &["auto", "low", "medium", "high"],
    )?;
    if size.as_deref().is_some_and(str::is_empty) {
        return Err(ImageRequestError::invalid(
            "'size' must not be empty",
            Some("size"),
        ));
    }
    model.get_or_insert_with(|| IMAGE_MODEL.to_string());
    background.get_or_insert_with(|| "auto".to_string());
    quality.get_or_insert_with(|| "auto".to_string());
    size.get_or_insert_with(|| "auto".to_string());
    Ok(())
}

fn validate_choice(
    field: &'static str,
    value: Option<&str>,
    allowed: &[&str],
) -> Result<(), ImageRequestError> {
    if let Some(value) = value
        && !allowed.contains(&value)
    {
        return Err(ImageRequestError::invalid(
            format!("Invalid '{field}' value '{value}'"),
            Some(field),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_edit_enforces_decoded_image_size_limits() {
        use base64::Engine as _;

        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(MAX_SINGLE_IMAGE_BYTES + 1, 0);
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let body = serde_json::to_vec(&serde_json::json!({
            "prompt": "x",
            "images": [{"image_url": data_url}]
        }))
        .unwrap();
        let error = prepare_json_request(ImageOperation::Edit, &body).unwrap_err();
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn auth_and_rate_limit_errors_use_openai_error_types() {
        use axum::body::to_bytes;

        for (status, expected_type) in [
            (StatusCode::UNAUTHORIZED, "authentication_error"),
            (StatusCode::FORBIDDEN, "permission_error"),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            (StatusCode::BAD_GATEWAY, "api_error"),
        ] {
            let response = image_error_response(ImageRequestError {
                status,
                message: "error".to_string(),
                param: None,
                code: None,
            });
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["error"]["type"],
                expected_type
            );
        }
    }

    #[test]
    fn request_validation_rejects_unsupported_and_unsafe_inputs() {
        for body in [
            br#"{"prompt":"x","model":"gpt-image-1"}"#.as_slice(),
            br#"{"prompt":"x","n":0}"#,
            br#"{"prompt":"x","response_format":"url"}"#,
            br#"{"prompt":" "}"#,
        ] {
            assert!(prepare_json_request(ImageOperation::Generation, body).is_err());
        }
        assert!(
            prepare_json_request(
                ImageOperation::Edit,
                br#"{"prompt":"x","images":[{"image_url":"https://example.com/x.png"}]}"#,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn backend_rejects_oversized_upstream_response_before_body_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                MAX_IMAGE_RESPONSE_BYTES + 1
            );
            stream.write_all(head.as_bytes()).await.unwrap();
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
        let backend =
            CodexImagesBackend::new_for_test(std::sync::Arc::new(client), format!("http://{addr}"));
        let response = backend
            .handle(
                ImageOperation::Generation,
                prepare_json_request(ImageOperation::Generation, br#"{"prompt":"x"}"#).unwrap(),
                crate::provider::RequestContext {
                    req_id: "oversized".into(),
                    session_id: None,
                    session_seq: None,
                    provider: "codex".into(),
                    traffic: None,
                    monitor: None,
                },
            )
            .await;
        server.await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn backend_passes_through_valid_bounded_image_json() {
        use axum::body::to_bytes;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 16 * 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = br#"{"created":1,"data":[{"b64_json":"aW1n"}],"quality":"medium"}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-request-id: upstream-1\r\nset-cookie: secret=1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(response).await.unwrap();
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
        let backend = CodexImagesBackend::new_for_test(
            std::sync::Arc::new(client),
            format!("http://{addr}/root"),
        );
        let prepared =
            prepare_json_request(ImageOperation::Generation, br#"{"prompt":"draw a fox"}"#)
                .unwrap();
        let response = backend
            .handle(
                ImageOperation::Generation,
                prepared,
                crate::provider::RequestContext {
                    req_id: "image-test".into(),
                    session_id: None,
                    session_seq: None,
                    provider: "codex".into(),
                    traffic: None,
                    monitor: None,
                },
            )
            .await;
        server.await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[http::header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-request-id"], "upstream-1");
        assert!(response.headers().get(http::header::SET_COOKIE).is_none());
        let body = to_bytes(response.into_body(), MAX_IMAGE_RESPONSE_BYTES)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["quality"],
            "medium"
        );
    }

    #[test]
    fn success_response_requires_created_and_nonempty_base64_items() {
        let valid = br#"{"created":1,"data":[{"b64_json":"aW1n"}],"usage":{"input_tokens":3}}"#;
        let usage = validate_success_response(valid).expect("valid response");
        assert_eq!(usage, (Some(3), None));

        assert!(validate_success_response(br#"{"data":[{"b64_json":"aW1n"}]}"#).is_err());
        assert!(validate_success_response(br#"{"created":1,"data":[]}"#).is_err());
        assert!(validate_success_response(br#"{"created":1,"data":[{"b64_json":""}]}"#).is_err());
    }

    #[test]
    fn production_image_base_url_is_locked_to_chatgpt_https() {
        assert_eq!(
            validate_image_base_url("https://chatgpt.com/backend-api/codex/").unwrap(),
            "https://chatgpt.com/backend-api/codex"
        );
        assert!(validate_image_base_url("http://chatgpt.com/backend-api/codex").is_err());
        assert!(validate_image_base_url("https://example.com/backend-api/codex").is_err());
        assert!(validate_image_base_url("https://chatgpt.com/backend-api/codex?x=1").is_err());
    }

    #[test]
    fn multipart_edit_is_translated_to_codex_data_urls() {
        let prepared = prepare_multipart_edit(MultipartEditInput {
            prompt: Some("make it blue".to_string()),
            model: None,
            background: None,
            n: None,
            quality: None,
            size: None,
            images: vec![UploadedImage {
                bytes: Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            }],
        })
        .expect("multipart edit should be valid");

        assert_eq!(prepared.image_count, 1);
        assert_eq!(
            prepared.body["images"][0]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn json_edit_request_accepts_data_urls_and_applies_defaults() {
        let prepared = prepare_json_request(
            ImageOperation::Edit,
            br#"{"prompt":"make it blue","images":[{"image_url":"data:image/png;base64,iVBORw0KGgo="}]}"#,
        )
        .expect("edit request should be valid");

        assert_eq!(prepared.model, IMAGE_MODEL);
        assert_eq!(prepared.image_count, 1);
        assert_eq!(
            prepared.body["images"][0]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
        assert_eq!(prepared.body["background"], "auto");
        assert_eq!(prepared.body["quality"], "auto");
        assert_eq!(prepared.body["size"], "auto");
    }

    #[test]
    fn generation_request_applies_safe_defaults() {
        let prepared =
            prepare_json_request(ImageOperation::Generation, br#"{"prompt":"draw a fox"}"#)
                .expect("generation request should be valid");

        assert_eq!(prepared.model, IMAGE_MODEL);
        assert_eq!(prepared.image_count, 0);
        assert_eq!(prepared.body["prompt"], "draw a fox");
        assert_eq!(prepared.body["model"], IMAGE_MODEL);
        assert_eq!(prepared.body["background"], "auto");
        assert_eq!(prepared.body["quality"], "auto");
        assert_eq!(prepared.body["size"], "auto");
        assert!(prepared.body.get("n").is_none());
    }
}
