//! Generation-only native HTTP. OAuth and refresh keep their separate reqwest client.

use super::{
    config::{CONNECT_TIMEOUT, Config},
    replay::Result,
};
use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use http_body_util::BodyExt;
use hyper::rt::{Read, ReadBufCursor, Write as HttpWrite};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{
    client::{
        legacy::{
            Client,
            connect::{
                Connected, Connection, HttpConnector,
                proxy::{SocksV4, SocksV5, Tunnel},
            },
        },
        proxy::matcher::Matcher,
    },
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use serde_json::Value;
use std::{
    convert::Infallible,
    error::Error,
    future::Future,
    io::{self, IoSlice, Write},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{net::TcpStream, time::Instant};
use tower::Service;

const MAX_TURN_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 8 * 1024;
const DECODE_INPUT_CHUNK: usize = 1024;
// Match agy's sendModelStreamRequest / idleTimeoutReadCloser (300s) and Codex's
// HTTP/WS header + body idle timeouts (300_000ms). 1M-context prefill and deep
// reasoning pauses on Carbon/Argon/Opus 5.5 routinely exceed 30s before headers
// or between SSE chunks, while TCP+TLS handshakes stay bounded by CONNECT_TIMEOUT.
pub(super) const HEADER_TIMEOUT: Duration = Duration::from_secs(300);
pub(super) const READ_IDLE: Duration = Duration::from_secs(300);
pub(super) const TURN_DEADLINE: Duration = Duration::from_secs(3600);
type BoxError = Box<dyn Error + Send + Sync>;
type Connecting =
    Pin<Box<dyn Future<Output = std::result::Result<MaybeHttpsStream<RouteIo>, BoxError>> + Send>>;
type TcpIo = TokioIo<TcpStream>;

pub(super) struct GenerationClient {
    client: Client<Connector, reqwest::Body>,
    native_headers: HeaderMap,
}

impl GenerationClient {
    pub(super) fn new(config: &Config) -> Result<Self> {
        Config::validate_endpoint(config.base_url.as_str(), cfg!(test))?;
        if config.base_url.path() != "/" {
            return Err("invalid native origin");
        }
        let connector = generation_connector()?;
        let builder = generation_client_builder(TokioExecutor::new());
        Ok(Self {
            client: builder.build(connector),
            native_headers: config.native_headers(),
        })
    }

    pub(super) async fn send(
        &self,
        endpoint: url::Url,
        access: &str,
        wire: &Value,
        header_deadline: Instant,
        request_deadline: Instant,
    ) -> Result<NativeResponse> {
        if endpoint.scheme() != "https" && !test_loopback_http(&endpoint) {
            return Err("invalid native endpoint");
        }
        // Preserve the existing prepared JSON/caller semantics; its source,
        // schema and replay structures were bounded by request::prepare.
        let bytes = serde_json::to_vec(wire).map_err(|_| "invalid native request JSON")?;
        let uri = endpoint
            .as_str()
            .parse::<Uri>()
            .map_err(|_| "invalid native endpoint")?;
        let body = reqwest::Body::wrap_stream(futures_util::stream::once(async move {
            Ok::<_, Infallible>(Bytes::from(bytes))
        }));
        let mut request = http::Request::builder()
            .method("POST")
            .uri(uri)
            .body(body)
            .map_err(|_| "invalid native request")?;
        *request.headers_mut() = self.native_headers.clone();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {access}"))
            .map_err(|_| "invalid native authorization")?;
        authorization.set_sensitive(true);
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, authorization);
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        request
            .headers_mut()
            .insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        // No reqwest client execution/default headers and no known byte SizeHint.
        // HTTP/1.1 chunking or HTTP/2 DATA is chosen by the negotiated protocol.
        if Instant::now() >= header_deadline || Instant::now() >= request_deadline {
            return Err("native headers deadline exceeded");
        }
        let response = self
            .client
            .request(request)
            .await
            .map_err(|_| "native headers transport failed")?;
        let (parts, body) = response.into_parts();
        let stream = body
            .into_data_stream()
            .map(|chunk| chunk.map_err(|_| BodyError::Transport))
            .boxed();
        Ok(NativeResponse::new(
            parts.status,
            parts.headers,
            stream,
            request_deadline,
        ))
    }
}

// Keep production and the owned-driver cancellation fixture on one policy.
// The executor is Hyper's existing boundary, not a transport/config preference.
fn generation_client_builder<E>(executor: E) -> hyper_util::client::legacy::Builder
where
    E: hyper::rt::Executor<Pin<Box<dyn Future<Output = ()> + Send>>>
        + Send
        + Sync
        + Clone
        + 'static,
{
    let mut builder = Client::builder(executor);
    builder
        .timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        .retry_canceled_requests(false)
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(Duration::from_secs(90));
    builder
}

fn generation_connector() -> Result<Connector> {
    let tls = tls_configuration()?;
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    http.set_keepalive(Some(Duration::from_secs(15)));
    http.set_keepalive_interval(Some(Duration::from_secs(15)));
    http.set_keepalive_retries(Some(3));
    let proxy_tls = HttpsConnectorBuilder::new()
        .with_tls_config(tls.clone())
        .https_only()
        .enable_http1()
        .wrap_connector(http.clone());
    let mut proxy_headers = HeaderMap::new();
    // Baseline reqwest CONNECT uses its client-builder identity, not the
    // per-request native UA override that belongs inside origin TLS.
    proxy_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(concat!("claude-code-proxy/", env!("CARGO_PKG_VERSION"))),
    );
    let route = RouteConnector {
        http: http.clone(),
        proxy_tls,
        proxy_headers,
        matcher: Arc::new(Matcher::from_system()),
    };
    let https = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http1()
        .enable_http2()
        .wrap_connector(route);
    Ok(Connector {
        https,
        #[cfg(test)]
        test_http: http,
    })
}

fn tls_configuration() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    let (valid, invalid) = roots.add_parsable_certificates(native.certs);
    // Same acceptance policy as the existing reqwest client: tolerate partial
    // native-store errors, but do not hide an entirely invalid native cert set.
    if valid == 0 && invalid > 0 {
        return Err("native TLS roots initialization failed");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| "native TLS versions initialization failed")?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn test_loopback_http(endpoint: &url::Url) -> bool {
    #[cfg(test)]
    {
        endpoint.scheme() == "http"
            && endpoint.host().is_some_and(|host| match host {
                url::Host::Ipv4(ip) => ip.is_loopback(),
                url::Host::Ipv6(ip) => ip.is_loopback(),
                _ => false,
            })
    }
    #[cfg(not(test))]
    {
        let _ = endpoint;
        false
    }
}

#[derive(Clone)]
struct Connector {
    https: HttpsConnector<RouteConnector>,
    #[cfg(test)]
    test_http: HttpConnector,
}

impl Service<Uri> for Connector {
    type Response = MaybeHttpsStream<RouteIo>;
    type Error = BoxError;
    type Future = Connecting;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), BoxError>> {
        self.https.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Connecting {
        #[cfg(test)]
        if uri.scheme_str() == Some("http") {
            let valid = uri
                .host()
                .and_then(|host| {
                    host.trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .is_some_and(|ip| ip.is_loopback());
            if valid {
                // Existing unit fixtures only. Never proxy cleartext Bearer;
                // normal-library integration tests use verified HTTPS instead.
                let connecting = self.test_http.call(uri);
                return Box::pin(connect_with_timeout(async move {
                    Ok(MaybeHttpsStream::Http(RouteIo::Plain(Box::new(
                        connecting.await?,
                    ))))
                }));
            }
        }
        let connecting = self.https.call(uri);
        Box::pin(connect_with_timeout(connecting))
    }
}

async fn connect_with_timeout<F, T>(future: F) -> std::result::Result<T, BoxError>
where
    F: Future<Output = std::result::Result<T, BoxError>>,
{
    tokio::time::timeout(CONNECT_TIMEOUT, future)
        .await
        .map_err(|_| {
            Box::new(io::Error::new(
                io::ErrorKind::TimedOut,
                "native connect timeout",
            )) as BoxError
        })?
}

#[derive(Clone)]
struct RouteConnector {
    http: HttpConnector,
    proxy_tls: HttpsConnector<HttpConnector>,
    proxy_headers: HeaderMap,
    matcher: Arc<Matcher>,
}

impl Service<Uri> for RouteConnector {
    type Response = RouteIo;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = std::result::Result<RouteIo, BoxError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::result::Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, destination: Uri) -> Self::Future {
        let mut http = self.http.clone();
        let proxy_tls = self.proxy_tls.clone();
        let proxy_headers = self.proxy_headers.clone();
        let intercepted = self.matcher.intercept(&destination);
        Box::pin(async move {
            let Some(proxy) = intercepted else {
                return Ok(RouteIo::Plain(Box::new(http.call(destination).await?)));
            };
            let scheme = proxy
                .uri()
                .scheme_str()
                .ok_or_else(|| io::Error::other("invalid native proxy scheme"))?;
            match scheme {
                "http" => {
                    let mut tunnel =
                        Tunnel::new(tcp_proxy_uri(proxy.uri())?, http).with_headers(proxy_headers);
                    if let Some(auth) = proxy.basic_auth() {
                        tunnel = tunnel.with_auth(auth.clone());
                    }
                    Ok(RouteIo::Plain(Box::new(tunnel.call(destination).await?)))
                }
                "https" => {
                    // The proxy TLS connector is HTTP/1 only for CONNECT. The
                    // independent outer HttpsConnector negotiates origin h1/h2.
                    let mut tunnel =
                        Tunnel::new(proxy.uri().clone(), proxy_tls).with_headers(proxy_headers);
                    if let Some(auth) = proxy.basic_auth() {
                        tunnel = tunnel.with_auth(auth.clone());
                    }
                    Ok(RouteIo::ProxyTls(Box::new(tunnel.call(destination).await?)))
                }
                "socks4" | "socks4a" => {
                    // Keep the locked legacy helper's wire behavior. Its extra
                    // NUL after an empty USERID is incompatible with strict RFC
                    // SOCKS4/4a peers; negative baseline-matching tests document
                    // that inherited limitation, not positive support.
                    let target = socks_destination(&destination, scheme).await?;
                    let mut socks = SocksV4::new(tcp_proxy_uri(proxy.uri())?, http);
                    Ok(RouteIo::Plain(Box::new(socks.call(target).await?)))
                }
                "socks5" | "socks5h" => {
                    let target = socks_destination(&destination, scheme).await?;
                    let mut socks = SocksV5::new(tcp_proxy_uri(proxy.uri())?, http);
                    if let Some((user, password)) = proxy.raw_auth() {
                        socks = socks.with_auth(user.to_owned(), password.to_owned());
                    }
                    Ok(RouteIo::Plain(Box::new(socks.call(target).await?)))
                }
                _ => Err(io::Error::other("unsupported native proxy scheme").into()),
            }
        })
    }
}

async fn socks_destination(destination: &Uri, scheme: &str) -> std::result::Result<Uri, BoxError> {
    if matches!(scheme, "socks4a" | "socks5h") {
        return Ok(destination.clone());
    }
    let host = destination
        .host()
        .ok_or_else(|| io::Error::other("native SOCKS destination missing"))?
        .trim_matches(['[', ']']);
    let port = destination.port_u16().unwrap_or(443);
    // The cached helpers' local_dns switch performs blocking getaddrinfo. Resolve
    // before their handshake so DNS is owned by the complete connect deadline.
    // Their Uri interface cannot encode an IPv6 literal as an IP address (host()
    // includes brackets); do not silently turn local DNS into proxy DNS.
    let address = tokio::net::lookup_host((host, port))
        .await?
        .find(|address| address.is_ipv4())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "native SOCKS local address unsupported",
            )
        })?;
    let mut parts = destination.clone().into_parts();
    parts.authority = Some(address.to_string().parse()?);
    Ok(Uri::from_parts(parts)?)
}

fn tcp_proxy_uri(proxy: &Uri) -> std::result::Result<Uri, BoxError> {
    let default_port = match proxy.scheme_str() {
        Some("http") => 80,
        Some("https") => 443,
        Some("socks4" | "socks4a" | "socks5" | "socks5h") => 1080,
        _ => return Err(io::Error::other("unsupported native proxy scheme").into()),
    };
    let host = proxy
        .host()
        .ok_or_else(|| io::Error::other("native proxy host missing"))?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(Uri::builder()
        .scheme("http")
        .authority(format!(
            "{host}:{}",
            proxy.port_u16().unwrap_or(default_port)
        ))
        .path_and_query("/")
        .build()?)
}

enum RouteIo {
    Plain(Box<TcpIo>),
    ProxyTls(Box<MaybeHttpsStream<TcpIo>>),
}

impl Connection for RouteIo {
    fn connected(&self) -> Connected {
        match self {
            Self::Plain(io) => io.connected(),
            Self::ProxyTls(io) => io.connected(),
        }
    }
}

impl Read for RouteIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io.as_mut()).poll_read(cx, buffer),
            Self::ProxyTls(io) => Pin::new(io.as_mut()).poll_read(cx, buffer),
        }
    }
}

impl HttpWrite for RouteIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io.as_mut()).poll_write(cx, bytes),
            Self::ProxyTls(io) => Pin::new(io.as_mut()).poll_write(cx, bytes),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io.as_mut()).poll_flush(cx),
            Self::ProxyTls(io) => Pin::new(io.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io.as_mut()).poll_shutdown(cx),
            Self::ProxyTls(io) => Pin::new(io.as_mut()).poll_shutdown(cx),
        }
    }
    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Plain(io) => io.is_write_vectored(),
            Self::ProxyTls(io) => io.is_write_vectored(),
        }
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io.as_mut()).poll_write_vectored(cx, bytes),
            Self::ProxyTls(io) => Pin::new(io.as_mut()).poll_write_vectored(cx, bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BodyError {
    ByteLimit,
    Idle,
    Deadline,
    Transport,
    Compression,
    Encoding,
}
impl BodyError {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::ByteLimit => "native turn byte limit",
            Self::Idle => "native stream idle timeout",
            Self::Deadline => "native turn deadline exceeded",
            Self::Transport => "native stream transport failed",
            Self::Compression => "invalid native gzip body",
            Self::Encoding => "unsupported native content encoding",
        }
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    total: usize,
    limit: usize,
    exceeded: bool,
}
impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            total: 0,
            limit,
            exceeded: false,
        }
    }
    fn take(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.bytes))
    }
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(total) = self
            .total
            .checked_add(bytes.len())
            .filter(|total| *total <= self.limit)
        else {
            self.exceeded = true;
            return Err(io::Error::other("native decoded byte limit"));
        };
        self.total = total;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Decoder {
    Identity,
    Gzip(Box<flate2::write::MultiGzDecoder<BoundedWriter>>),
}

/// Original headers/hint describe ENCODED bytes. The typed reader yields only
/// validated decoded bytes and never erases byte/idle/deadline failures into a
/// reqwest::Error. Status/content-type remain available before any body work.
pub(crate) struct NativeResponse {
    status: StatusCode,
    headers: HeaderMap,
    encoded_hint: Option<u64>,
    body: BoxStream<'static, std::result::Result<Bytes, BodyError>>,
    remaining: Bytes,
    encoded: usize,
    limit: usize,
    decoder: Decoder,
    initial_error: Option<BodyError>,
    deadline: Instant,
    finished: bool,
}

impl NativeResponse {
    fn new(
        status: StatusCode,
        headers: HeaderMap,
        body: BoxStream<'static, std::result::Result<Bytes, BodyError>>,
        deadline: Instant,
    ) -> Self {
        let limit = if status.is_success() {
            MAX_TURN_BYTES
        } else {
            MAX_ERROR_BYTES
        };
        let hint = encoded_hint(&headers);
        let encoding = response_encoding(&headers);
        let initial_error = match (hint.as_ref(), encoding.as_ref()) {
            (Ok(Some(length)), _) if *length > limit as u64 => Some(BodyError::ByteLimit),
            (Err(error), _) | (_, Err(error)) => Some(*error),
            _ => None,
        };
        let decoder = if encoding == Ok(true) {
            Decoder::Gzip(Box::new(flate2::write::MultiGzDecoder::new(
                BoundedWriter::new(limit),
            )))
        } else {
            Decoder::Identity
        };
        Self {
            status,
            headers,
            encoded_hint: hint.ok().flatten(),
            body,
            remaining: Bytes::new(),
            encoded: 0,
            limit,
            decoder,
            initial_error,
            deadline,
            finished: false,
        }
    }
    pub(super) fn status(&self) -> StatusCode {
        self.status
    }
    pub(super) fn headers(&self) -> &HeaderMap {
        &self.headers
    }
    pub(super) fn encoded_length_hint(&self) -> Option<u64> {
        self.encoded_hint
    }

    fn fail(&mut self, error: BodyError) -> std::result::Result<Option<Bytes>, BodyError> {
        self.finished = true;
        self.remaining = Bytes::new();
        self.body = futures_util::stream::empty().boxed();
        Err(error)
    }

    pub(super) async fn chunk(&mut self) -> std::result::Result<Option<Bytes>, BodyError> {
        if let Some(error) = self.initial_error.take() {
            return self.fail(error);
        }
        if self.finished {
            return Ok(None);
        }
        let mut work = 0;
        loop {
            if Instant::now() >= self.deadline {
                return self.fail(BodyError::Deadline);
            }
            if self.remaining.is_empty() {
                let idle = (Instant::now() + READ_IDLE).min(self.deadline);
                let next = match tokio::time::timeout_at(idle, self.body.next()).await {
                    Ok(Some(Ok(chunk))) => Some(chunk),
                    Ok(Some(Err(error))) => return self.fail(error),
                    Ok(None) => None,
                    Err(_) => {
                        return self.fail(if Instant::now() >= self.deadline {
                            BodyError::Deadline
                        } else {
                            BodyError::Idle
                        });
                    }
                };
                let Some(chunk) = next else {
                    self.finished = true;
                    return match &mut self.decoder {
                        Decoder::Identity => Ok(None),
                        Decoder::Gzip(decoder) => {
                            if decoder.header().is_none() || decoder.try_finish().is_err() {
                                Err(if decoder.get_ref().exceeded {
                                    BodyError::ByteLimit
                                } else {
                                    BodyError::Compression
                                })
                            } else {
                                let bytes = decoder.get_mut().take();
                                Ok((!bytes.is_empty()).then_some(bytes))
                            }
                        }
                    };
                };
                let Some(total) = self
                    .encoded
                    .checked_add(chunk.len())
                    .filter(|size| *size <= self.limit)
                else {
                    return self.fail(BodyError::ByteLimit);
                };
                self.encoded = total;
                self.remaining = chunk;
                if self.remaining.is_empty() {
                    work += 1;
                    if work == 32 {
                        tokio::task::yield_now().await;
                        work = 0;
                    }
                    continue;
                }
            }
            let chunk = self
                .remaining
                .split_to(self.remaining.len().min(DECODE_INPUT_CHUNK));
            let output = match &mut self.decoder {
                Decoder::Identity => chunk,
                Decoder::Gzip(decoder) => {
                    if decoder.write_all(&chunk).is_err() {
                        let error = if decoder.get_ref().exceeded {
                            BodyError::ByteLimit
                        } else {
                            BodyError::Compression
                        };
                        return self.fail(error);
                    }
                    decoder.get_mut().take()
                }
            };
            if !output.is_empty() {
                return Ok(Some(output));
            }
            work += 1;
            if work == 32 {
                tokio::task::yield_now().await;
                work = 0;
            }
        }
    }
}

fn encoded_hint(headers: &HeaderMap) -> std::result::Result<Option<u64>, BodyError> {
    let mut length = None;
    for value in headers.get_all(header::CONTENT_LENGTH).iter() {
        for value in value.to_str().map_err(|_| BodyError::Transport)?.split(',') {
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(BodyError::Transport);
            }
            let parsed = value.parse::<u64>().map_err(|_| BodyError::Transport)?;
            if length.is_some_and(|old| old != parsed) {
                return Err(BodyError::Transport);
            }
            length = Some(parsed);
        }
    }
    Ok(length)
}

fn response_encoding(headers: &HeaderMap) -> std::result::Result<bool, BodyError> {
    let mut values = headers.get_all(header::CONTENT_ENCODING).iter();
    let Some(value) = values.next() else {
        return Ok(false);
    };
    if values.next().is_some() {
        return Err(BodyError::Encoding);
    }
    let value = value.to_str().map_err(|_| BodyError::Encoding)?.trim();
    if value.eq_ignore_ascii_case("identity") {
        Ok(false)
    } else if value.eq_ignore_ascii_case("gzip") {
        Ok(true)
    } else {
        Err(BodyError::Encoding)
    }
}

#[cfg(test)]
impl From<reqwest::Response> for NativeResponse {
    fn from(response: reqwest::Response) -> Self {
        let status = response.status();
        let headers = response.headers().clone();
        let stream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|_| BodyError::Transport))
            .boxed();
        Self::new(
            status,
            headers,
            stream,
            Instant::now() + Duration::from_secs(180),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn response(status: u16, encoding: Option<&str>, bytes: &[u8], chunk: usize) -> NativeResponse {
        let mut headers = HeaderMap::new();
        if let Some(encoding) = encoding {
            headers.insert("content-encoding", HeaderValue::from_str(encoding).unwrap());
        }
        let chunks = bytes
            .chunks(chunk)
            .map(Bytes::copy_from_slice)
            .map(Ok)
            .collect::<Vec<_>>();
        NativeResponse::new(
            StatusCode::from_u16(status).unwrap(),
            headers,
            futures_util::stream::iter(chunks).boxed(),
            Instant::now() + Duration::from_secs(180),
        )
    }

    async fn decoded(mut response: NativeResponse) -> std::result::Result<Vec<u8>, BodyError> {
        let mut result = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            result.extend_from_slice(&chunk);
        }
        Ok(result)
    }

    #[tokio::test]
    async fn antigravity_generation_gzip_fragments_require_complete_members() {
        let first = gzip(b"first");
        let mut concatenated = first.clone();
        concatenated.extend(gzip(b"second"));
        for chunk in [1, 3, 16, 4096] {
            assert_eq!(
                decoded(response(200, Some("GZip"), &concatenated, chunk))
                    .await
                    .unwrap(),
                b"firstsecond"
            );
            let mut crc = first.clone();
            let index = crc.len() - 8;
            crc[index] ^= 1;
            let mut truncated = first.clone();
            truncated.pop();
            let mut junk = first.clone();
            junk.extend_from_slice(b"trailing");
            let mut partial = first.clone();
            partial.extend_from_slice(&[0x1f, 0x8b]);
            for bytes in [Vec::new(), crc, truncated, junk, partial] {
                assert_eq!(
                    decoded(response(200, Some("gzip"), &bytes, chunk))
                        .await
                        .unwrap_err(),
                    BodyError::Compression
                );
            }
        }
    }

    #[tokio::test]
    async fn antigravity_generation_error_decode_limit_is_not_transport_error() {
        let encoded = gzip(&vec![b'x'; MAX_ERROR_BYTES + 1]);
        assert!(encoded.len() < MAX_ERROR_BYTES);
        assert_eq!(
            decoded(response(429, Some("gzip"), &encoded, 1))
                .await
                .unwrap_err(),
            BodyError::ByteLimit
        );
        assert_eq!(
            decoded(response(200, Some("identity"), b"plain", 1))
                .await
                .unwrap(),
            b"plain"
        );
    }

    #[test]
    fn antigravity_generation_bounded_sink_rejects_before_allocation() {
        let mut sink = BoundedWriter::new(8);
        assert!(sink.write_all(b"123456789").is_err());
        assert!(sink.bytes.is_empty());
        assert!(sink.exceeded);
        let mut sink = BoundedWriter::new(8);
        sink.write_all(b"12345678").unwrap();
        assert!(sink.write_all(b"9").is_err());
        assert_eq!(sink.bytes.len(), 8);
    }

    #[tokio::test]
    async fn antigravity_generation_original_encoded_hint_prevents_polling() {
        for (status, limit) in [
            (StatusCode::OK, MAX_TURN_BYTES),
            (StatusCode::TOO_MANY_REQUESTS, MAX_ERROR_BYTES),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            let stream = futures_util::stream::poll_fn(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(None)
            })
            .boxed();
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-length",
                HeaderValue::from_str(&(limit + 1).to_string()).unwrap(),
            );
            headers.insert("content-encoding", HeaderValue::from_static("gzip"));
            headers.insert("retry-after", HeaderValue::from_static("7"));
            let mut native = NativeResponse::new(
                status,
                headers,
                stream,
                Instant::now() + Duration::from_secs(180),
            );
            assert_eq!(native.encoded_length_hint(), Some((limit + 1) as u64));
            assert_eq!(native.status(), status);
            assert_eq!(native.headers()["retry-after"], "7");
            assert_eq!(native.chunk().await.unwrap_err(), BodyError::ByteLimit);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn antigravity_generation_all_content_encoding_values_are_validated() {
        for values in [
            vec!["gzip", "gzip"],
            vec!["identity", "gzip"],
            vec!["gzip, identity"],
            vec!["br"],
            vec![""],
        ] {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append("content-encoding", HeaderValue::from_str(value).unwrap());
            }
            let mut native = NativeResponse::new(
                StatusCode::OK,
                headers,
                futures_util::stream::empty().boxed(),
                Instant::now() + Duration::from_secs(180),
            );
            assert_eq!(native.chunk().await.unwrap_err(), BodyError::Encoding);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn antigravity_generation_idle_and_total_errors_remain_typed() {
        for (budget, expected, elapsed) in [
            (3600, BodyError::Idle, READ_IDLE),
            (180, BodyError::Deadline, Duration::from_secs(180)),
            (10, BodyError::Deadline, Duration::from_secs(10)),
        ] {
            let started = Instant::now();
            let mut native = NativeResponse::new(
                StatusCode::OK,
                HeaderMap::new(),
                futures_util::stream::pending().boxed(),
                started + Duration::from_secs(budget),
            );
            assert_eq!(native.chunk().await.unwrap_err(), expected);
            assert_eq!(Instant::now() - started, elapsed);
        }
        assert_eq!(BodyError::ByteLimit.message(), "native turn byte limit");
        assert_eq!(BodyError::Idle.message(), "native stream idle timeout");
        assert_eq!(
            BodyError::Deadline.message(),
            "native turn deadline exceeded"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn antigravity_generation_long_reasoning_pause_under_idle_budget_succeeds() {
        let started = Instant::now();
        let stream = futures_util::stream::unfold(0_usize, |step| async move {
            match step {
                0 => {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                    Some((Ok(Bytes::from_static(b"data: first\n\n")), 1))
                }
                1 => {
                    tokio::time::sleep(Duration::from_secs(240)).await;
                    Some((Ok(Bytes::from_static(b"data: second\n\n")), 2))
                }
                _ => None,
            }
        })
        .boxed();
        let mut native = NativeResponse::new(
            StatusCode::OK,
            HeaderMap::new(),
            stream,
            started + TURN_DEADLINE,
        );
        assert_eq!(
            native.chunk().await.unwrap().unwrap(),
            Bytes::from_static(b"data: first\n\n")
        );
        assert_eq!(
            native.chunk().await.unwrap().unwrap(),
            Bytes::from_static(b"data: second\n\n")
        );
        assert_eq!(native.chunk().await.unwrap(), None);
        assert_eq!(Instant::now() - started, Duration::from_secs(360));
    }

    #[tokio::test(start_paused = true)]
    async fn antigravity_generation_gzip_header_progress_never_renews_total_deadline() {
        let started = Instant::now();
        let stream = futures_util::stream::unfold(0_usize, |index| async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Some((
                Ok(Bytes::from_static(if index == 0 {
                    &[0x1f]
                } else {
                    &[0x8b]
                })),
                index + 1,
            ))
        })
        .boxed();
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        let mut native = NativeResponse::new(
            StatusCode::OK,
            headers,
            stream,
            started + Duration::from_secs(12),
        );
        assert_eq!(native.chunk().await.unwrap_err(), BodyError::Deadline);
        assert_eq!(Instant::now() - started, Duration::from_secs(12));
    }

    #[tokio::test(start_paused = true)]
    async fn antigravity_generation_connect_budget_covers_whole_connector_future() {
        let started = Instant::now();
        let result = connect_with_timeout(futures_util::future::pending::<
            std::result::Result<(), BoxError>,
        >())
        .await;
        assert!(result.is_err());
        assert_eq!(Instant::now() - started, CONNECT_TIMEOUT);
    }

    #[tokio::test]
    async fn antigravity_generation_local_socks_dns_is_resolved_before_handshake() {
        let destination: Uri = "https://localhost:443/".parse().unwrap();
        for local in ["socks4", "socks5"] {
            let resolved = socks_destination(&destination, local).await.unwrap();
            assert!(
                resolved
                    .host()
                    .unwrap()
                    .parse::<std::net::Ipv4Addr>()
                    .unwrap()
                    .is_loopback()
            );
        }
        for remote in ["socks4a", "socks5h"] {
            assert_eq!(
                socks_destination(&destination, remote).await.unwrap(),
                destination
            );
        }
    }

    #[tokio::test]
    async fn antigravity_generation_drop_cancels_partial_gzip_success_and_error_readers() {
        struct DropGuard(Arc<AtomicUsize>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        for status in [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let guard = DropGuard(dropped.clone());
            let mut first = true;
            let body = futures_util::stream::poll_fn(move |_| {
                let _owned = &guard;
                if first {
                    first = false;
                    Poll::Ready(Some(Ok(Bytes::from_static(&[0x1f]))))
                } else {
                    Poll::Pending
                }
            })
            .boxed();
            let mut headers = HeaderMap::new();
            headers.insert("content-encoding", HeaderValue::from_static("gzip"));
            let mut native = NativeResponse::new(
                status,
                headers,
                body,
                Instant::now() + Duration::from_secs(180),
            );
            let mut reader = Box::pin(async move { native.chunk().await });
            assert!(futures_util::poll!(reader.as_mut()).is_pending());
            assert_eq!(dropped.load(Ordering::SeqCst), 0);
            drop(reader);
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                1,
                "partial gzip state and owned upstream must drop without a detached collector"
            );
        }
    }

    #[tokio::test]
    async fn antigravity_generation_empty_frames_yield_before_unbounded_work() {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let stream = futures_util::stream::poll_fn(move |_| {
            let count = observed.fetch_add(1, Ordering::SeqCst);
            if count < 4096 {
                Poll::Ready(Some(Ok(Bytes::new())))
            } else {
                Poll::Pending
            }
        })
        .boxed();
        let mut native = NativeResponse::new(
            StatusCode::OK,
            HeaderMap::new(),
            stream,
            Instant::now() + Duration::from_secs(180),
        );
        let future = native.chunk();
        tokio::pin!(future);
        assert!(futures_util::poll!(future.as_mut()).is_pending());
        assert!(
            polls.load(Ordering::SeqCst) <= 32,
            "empty encoded frames must yield within bounded work"
        );
    }

    #[tokio::test]
    async fn antigravity_generation_idle_socket_closes_without_a_followup_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin =
            url::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let service = hyper::service::service_fn(
                |request: http::Request<hyper::body::Incoming>| async move {
                    let mut body = request.into_body();
                    let mut count = 0;
                    while let Some(frame) = body.frame().await {
                        if let Ok(bytes) = frame.unwrap().into_data() {
                            count += bytes.len();
                            assert!(count <= 4096);
                        }
                    }
                    Ok::<_, Infallible>(
                        http::Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(http_body_util::Full::new(Bytes::from_static(b"idle")))
                            .unwrap(),
                    )
                },
            );
            let closed = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .await
                .is_ok();
            let _ = closed_tx.send(closed);
        });
        let client = GenerationClient::new(&Config {
            base_url: origin.clone(),
            ..Config::default()
        })
        .unwrap();
        let started = Instant::now();
        let mut native = client
            .send(
                origin,
                "public-test-access",
                &serde_json::json!({"synthetic":"idle"}),
                started + Duration::from_secs(30),
                started + Duration::from_secs(180),
            )
            .await
            .unwrap();
        while native.chunk().await.unwrap().is_some() {}
        drop(native);
        // Register the actual idle entry/task before shifting only this runtime's
        // clock. Keep the client alive and issue NO second request.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(91)).await;
        tokio::task::yield_now().await;
        tokio::time::resume();
        let closed = matches!(
            tokio::time::timeout(Duration::from_secs(1), closed_rx).await,
            Ok(Ok(true))
        );
        if !closed {
            server.abort();
        }
        assert!(
            closed,
            "idle pool must close its socket without a followup request"
        );
        server.await.unwrap();
        drop(client);
    }

    // These executor futures are owned and polled by the test, never detached.
    // In the cached legacy client the first execute() is the H1 connection
    // driver; later futures are pool readiness/idle work. Bound all of them.
    type Driver = Pin<Box<dyn Future<Output = ()> + Send>>;
    const MAX_QUEUE_TEST_TASKS: usize = 8;

    #[derive(Default)]
    struct QueueDriverWake {
        parent: futures_util::task::AtomicWaker,
        wakes: AtomicUsize,
    }

    impl futures_util::task::ArcWake for QueueDriverWake {
        fn wake_by_ref(wake: &Arc<Self>) {
            wake.wakes.fetch_add(1, Ordering::SeqCst);
            wake.parent.wake();
        }
    }

    #[derive(Clone, Default)]
    struct QueueExecutor {
        pending: Arc<std::sync::Mutex<Vec<Driver>>>,
        scheduled: Arc<AtomicUsize>,
    }

    impl<F> hyper::rt::Executor<F> for QueueExecutor
    where
        F: Future<Output = ()> + Send + 'static,
    {
        fn execute(&self, future: F) {
            let count = self.scheduled.fetch_add(1, Ordering::SeqCst);
            assert!(count < MAX_QUEUE_TEST_TASKS, "bounded queue fixture tasks");
            self.pending.lock().unwrap().push(Box::pin(future));
        }
    }

    struct QueueDrivers {
        executor: QueueExecutor,
        tasks: Vec<Option<Driver>>,
        wake: Arc<QueueDriverWake>,
        driver_polls: usize,
        paused: bool,
    }

    impl QueueDrivers {
        fn new(executor: QueueExecutor) -> Self {
            Self {
                executor,
                tasks: Vec::new(),
                wake: Arc::default(),
                driver_polls: 0,
                paused: false,
            }
        }

        fn poll_tasks(&mut self, cx: &mut Context<'_>) {
            self.wake.parent.register(cx.waker());
            // Include helper futures scheduled while polling, without holding
            // the executor mutex across a poll or polling any task twice.
            for index in 0..MAX_QUEUE_TEST_TASKS {
                self.tasks
                    .extend(self.executor.pending.lock().unwrap().drain(..).map(Some));
                let Some(slot) = self.tasks.get_mut(index) else {
                    break;
                };
                let Some(task) = slot.as_mut() else {
                    continue;
                };
                let result = if index == 0 {
                    if self.paused {
                        continue;
                    }
                    self.driver_polls += 1;
                    let wake = futures_util::task::waker_ref(&self.wake);
                    task.as_mut().poll(&mut Context::from_waker(&wake))
                } else {
                    task.as_mut().poll(cx)
                };
                if result.is_ready() {
                    *slot = None;
                }
            }
        }

        async fn run<F: Future>(&mut self, future: F) -> F::Output {
            tokio::pin!(future);
            futures_util::future::poll_fn(|cx| {
                let result = future.as_mut().poll(cx);
                // Also settle the driver/readiness helpers when the operation
                // completes, so a fully consumed warmup can enter the pool.
                self.poll_tasks(cx);
                result
            })
            .await
        }

        fn pause_ack(&mut self) -> (usize, usize) {
            assert!(self.tasks.first().is_some_and(Option::is_some));
            self.paused = true;
            (self.driver_polls, self.wake.wakes.load(Ordering::SeqCst))
        }

        fn cancel_paused_driver(&mut self) {
            assert!(self.paused);
            // Dropping Hyper's queued Envelope returns the untouched request
            // with Error::Canceled; do not poll/write this driver after pause.
            drop(self.tasks[0].take().expect("owned paused H1 driver"));
        }

        async fn finish(mut self) {
            self.paused = false;
            let finished = tokio::time::timeout(
                Duration::from_secs(1),
                futures_util::future::poll_fn(|cx| {
                    self.poll_tasks(cx);
                    if self.tasks.iter().all(Option::is_none)
                        && self.executor.pending.lock().unwrap().is_empty()
                    {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }),
            )
            .await;
            // Every future is still owned here even on timeout; dropping this
            // set cancels it synchronously. No spawned driver can outlive it.
            assert!(finished.is_ok(), "owned client drivers must finish");
        }
    }

    struct QueueOriginTask(Option<tokio::task::JoinHandle<()>>);

    impl Drop for QueueOriginTask {
        fn drop(&mut self) {
            if let Some(task) = &self.0 {
                task.abort();
            }
        }
    }

    async fn reused_canceled_queue_case(retry_counterfactual: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin: Uri = format!("http://{}/native", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let tcp = Arc::new(AtomicUsize::new(0));
        let native = Arc::new(AtomicUsize::new(0));
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let observed_tcp = tcp.clone();
        let observed_native = native.clone();
        let mut server = QueueOriginTask(Some(tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let socket = tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => accepted.unwrap().0,
                };
                assert!(observed_tcp.fetch_add(1, Ordering::SeqCst) < 2);
                let native = observed_native.clone();
                connections.spawn(async move {
                    let service = hyper::service::service_fn(
                        move |request: http::Request<hyper::body::Incoming>| {
                            let native = native.clone();
                            async move {
                                assert_eq!(request.method(), http::Method::POST);
                                assert_eq!(request.uri().path(), "/native");
                                // Count native HEADERS, separately from TCP and
                                // before consuming the bounded synthetic body.
                                assert!(native.fetch_add(1, Ordering::SeqCst) < 2);
                                let mut body = request.into_body();
                                let mut bytes = Vec::new();
                                let mut frames = 0;
                                while let Some(frame) = body.frame().await {
                                    frames += 1;
                                    assert!(frames <= 8);
                                    if let Ok(data) = frame.unwrap().into_data() {
                                        assert!(bytes.len() + data.len() <= 4096);
                                        bytes.extend_from_slice(&data);
                                    }
                                }
                                assert_eq!(bytes, b"{\"synthetic\":\"queue\"}");
                                Ok::<_, Infallible>(http::Response::new(http_body_util::Full::new(
                                    Bytes::from_static(b"ok"),
                                )))
                            }
                        },
                    );
                    // Driver cancellation may close the first idle socket.
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
            connections.abort_all();
            while let Some(joined) = connections.join_next().await {
                assert!(joined.is_ok() || joined.unwrap_err().is_cancelled());
            }
        })));
        let executor = QueueExecutor::default();
        let mut drivers = QueueDrivers::new(executor.clone());
        let mut builder = generation_client_builder(executor);
        if retry_counterfactual {
            // Only this synthetic counterfactual differs from production policy.
            builder.retry_canceled_requests(true);
        }
        let client: Client<_, reqwest::Body> = builder.build(generation_connector().unwrap());
        let request = || {
            http::Request::builder()
                .method("POST")
                .uri(origin.clone())
                .body(reqwest::Body::wrap_stream(futures_util::stream::once(
                    async { Ok::<_, Infallible>(Bytes::from_static(b"{\"synthetic\":\"queue\"}")) },
                )))
                .unwrap()
        };
        tokio::time::timeout(
            Duration::from_secs(2),
            drivers.run(async {
                let response = client.request(request()).await.unwrap();
                assert_eq!(response.version(), http::Version::HTTP_11);
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    "ok"
                );
            }),
        )
        .await
        .expect("bounded actual H1 warmup");
        assert_eq!(tcp.load(Ordering::SeqCst), 1);
        assert_eq!(native.load(Ordering::SeqCst), 1);
        let (paused_polls, before_enqueue_wakes) = drivers.pause_ack();
        let mut second = Box::pin(client.request(request()));
        // No await/yield between this snapshot, one request poll, and the wake
        // observation. On this current-thread runtime no timer, socket reactor,
        // or helper task can produce an unrelated wake in this interval.
        let request_wake = futures_util::task::noop_waker();
        let mut request_cx = Context::from_waker(&request_wake);
        assert!(second.as_mut().poll(&mut request_cx).is_pending());
        assert!(drivers.wake.wakes.load(Ordering::SeqCst) > before_enqueue_wakes);
        assert_eq!(drivers.driver_polls, paused_polls);
        assert_eq!(tcp.load(Ordering::SeqCst), 1, "no second TCP while gated");
        assert_eq!(
            native.load(Ordering::SeqCst),
            1,
            "second HEADERS not dispatched"
        );
        drivers.cancel_paused_driver();
        let result = tokio::time::timeout(Duration::from_secs(2), drivers.run(second.as_mut()))
            .await
            .expect("bounded canceled-queue outcome");
        if retry_counterfactual {
            let response = result.expect("retry=true must recover the untouched reused request");
            assert_eq!(response.status(), StatusCode::OK);
            let body = tokio::time::timeout(
                Duration::from_secs(2),
                drivers.run(response.into_body().collect()),
            )
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
            assert_eq!(body, "ok");
        } else {
            let error = result.expect_err("production retry=false must be terminal");
            assert!(!error.is_connect(), "not a cold connect failure");
            assert!(error.connect_info().is_some());
            assert!(
                error.source().is_some_and(|source| {
                    source
                        .downcast_ref::<hyper::Error>()
                        .is_some_and(hyper::Error::is_canceled)
                }),
                "queued request must produce Hyper cancellation"
            );
        }
        let expected_total = if retry_counterfactual { 2 } else { 1 };
        // Totals INCLUDE the one successful warmup. Second-request-only counts
        // are respectively zero with production policy and one with retry=true.
        assert_eq!(tcp.load(Ordering::SeqCst), expected_total);
        assert_eq!(native.load(Ordering::SeqCst), expected_total);
        assert_eq!(
            tcp.load(Ordering::SeqCst) - 1,
            usize::from(retry_counterfactual)
        );
        assert_eq!(
            native.load(Ordering::SeqCst) - 1,
            usize::from(retry_counterfactual)
        );
        drop(second);
        drop(client);
        drivers.finish().await;
        stop_tx.send(()).unwrap();
        let mut server_task = server.0.take().unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(1), &mut server_task).await;
        if stopped.is_err() {
            server_task.abort();
            let _ = server_task.await;
            panic!("bounded owned origin shutdown");
        }
        stopped.unwrap().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn antigravity_generation_reused_canceled_queue_is_not_retried() {
        reused_canceled_queue_case(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn antigravity_generation_reused_canceled_queue_retry_counterfactual() {
        reused_canceled_queue_case(true).await;
    }

    #[test]
    fn antigravity_generation_proxy_tcp_authorities_and_default_ports() {
        for (uri, expected) in [
            ("socks4://localhost/", "http://localhost:1080/"),
            ("socks4a://localhost:123/", "http://localhost:123/"),
            ("socks5://127.0.0.1/", "http://127.0.0.1:1080/"),
            ("socks5h://[::1]/", "http://[::1]:1080/"),
            ("http://localhost/", "http://localhost:80/"),
            ("https://localhost/", "http://localhost:443/"),
        ] {
            assert_eq!(
                tcp_proxy_uri(&uri.parse().unwrap()).unwrap().to_string(),
                expected
            );
        }
        assert!(tcp_proxy_uri(&"ftp://localhost/".parse().unwrap()).is_err());
    }
}
