use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

mod mock;

pub use mock::{MockMonitor, mock_state};

const DEFAULT_RECENT_LIMIT: usize = 200;
pub const SESSION_TOKEN_BUCKET_SECS: u64 = 10;
const SESSION_TOKEN_HISTORY_WINDOW_SECS: u64 = 60 * 60;
const SESSION_TOKEN_HISTORY_BUCKET_LIMIT: usize =
    (SESSION_TOKEN_HISTORY_WINDOW_SECS / SESSION_TOKEN_BUCKET_SECS) as usize;
const SESSION_TOKEN_HISTORY_SESSION_LIMIT: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Messages,
    CountTokens,
}

impl EndpointKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    Started,
    ProviderSelected,
    Upstream,
    Streaming,
    Completed,
    Failed,
}

impl RequestStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::ProviderSelected => "selected",
            Self::Upstream => "upstream",
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    RequestStarted {
        request_id: String,
        session_id: Option<String>,
        session_seq: Option<u64>,
        endpoint: EndpointKind,
    },
    ProjectResolved {
        request_id: String,
        project: String,
    },
    SessionSequenceResolved {
        request_id: String,
        session_seq: u64,
    },
    ProviderSelected {
        request_id: String,
        provider: String,
        model: String,
        effort: Option<String>,
    },
    ModelResolved {
        request_id: String,
        model: String,
    },
    UpstreamStarted {
        request_id: String,
    },
    GenerationStarted {
        request_id: String,
    },
    TrafficCapturePath {
        request_id: String,
        path: PathBuf,
    },
    StreamProgress {
        request_id: String,
        bytes: u64,
        chunks: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    UsageUpdated {
        request_id: String,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    RequestCompleted {
        request_id: String,
        http_status: u16,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
    RequestFailed {
        request_id: String,
        http_status: Option<u16>,
        error: String,
    },
    RequestAbandoned {
        request_id: String,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct ActiveRequest {
    pub request_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    started_instant: Instant,
    pub generation_started_at: Option<SystemTime>,
    generation_started_instant: Option<Instant>,
    generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
}

impl ActiveRequest {
    pub fn elapsed(&self) -> Duration {
        self.started_instant.elapsed()
    }

    pub fn rate(&self) -> Throughput {
        throughput(
            self.output_tokens
                .and_then(|tokens| tokens.checked_sub(self.generation_initial_output_tokens)),
            self.streamed_bytes,
            self.stream_chunks,
            self.generation_duration.unwrap_or(Duration::ZERO),
        )
    }
}

#[derive(Debug, Clone)]
pub struct CompletedRequest {
    pub request_id: String,
    pub session_id: Option<String>,
    pub session_seq: Option<u64>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub endpoint: EndpointKind,
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    pub generation_started_at: Option<SystemTime>,
    generation_started_instant: Option<Instant>,
    generation_initial_output_tokens: u64,
    pub generation_finished_at: Option<SystemTime>,
    pub generation_duration: Option<Duration>,
    pub status: RequestStatus,
    pub http_status: Option<u16>,
    pub latency: Duration,
    pub streamed_bytes: u64,
    pub stream_chunks: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub error: Option<String>,
    pub traffic_capture_path: Option<PathBuf>,
}

impl CompletedRequest {
    pub fn rate(&self) -> Throughput {
        throughput(
            self.output_tokens
                .and_then(|tokens| tokens.checked_sub(self.generation_initial_output_tokens)),
            self.streamed_bytes,
            self.stream_chunks,
            self.generation_duration.unwrap_or(Duration::ZERO),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Throughput {
    TokensPerSecond(f64),
    BytesPerSecond(f64),
    EventsPerSecond(f64),
    None,
}

impl Throughput {
    pub fn label(&self) -> String {
        match self {
            Self::TokensPerSecond(value) => format!("{value:.1} tok/s"),
            Self::BytesPerSecond(value) if *value >= 1024.0 => {
                format!("{:.1} KB/s", value / 1024.0)
            }
            Self::BytesPerSecond(value) => format!("{value:.0} B/s"),
            Self::EventsPerSecond(value) => format!("{value:.1} ev/s"),
            Self::None => "-".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MonitorState {
    pub started_at: SystemTime,
    pub sessions: Vec<SessionSummary>,
    pub active: Vec<ActiveRequest>,
    pub recent: Vec<CompletedRequest>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub active_count: usize,
    pub request_count: usize,
    pub failure_count: usize,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub last_seen: SystemTime,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub output_token_samples: Vec<(SystemTime, u64)>,
    rate_output_tokens: u64,
    pub generation_duration: Duration,
    pub last_status: String,
}

impl SessionSummary {
    pub fn rate(&self) -> Throughput {
        throughput(
            Some(self.rate_output_tokens).filter(|tokens| *tokens > 0),
            0,
            0,
            self.generation_duration,
        )
    }

    pub fn label(&self) -> String {
        self.session_id
            .clone()
            .unwrap_or_else(|| "no-session".to_string())
    }
}

#[derive(Debug)]
struct MonitorStore {
    started_at: SystemTime,
    active: HashMap<String, ActiveRequest>,
    recent: VecDeque<CompletedRequest>,
    session_output_buckets: HashMap<Option<String>, Vec<(u64, u64)>>,
    recent_limit: usize,
}

#[derive(Debug, Clone)]
pub struct MonitorHandle {
    store: Arc<Mutex<MonitorStore>>,
}

impl Default for MonitorHandle {
    fn default() -> Self {
        Self::new(DEFAULT_RECENT_LIMIT)
    }
}

impl MonitorHandle {
    pub fn new(recent_limit: usize) -> Self {
        Self {
            store: Arc::new(Mutex::new(MonitorStore {
                started_at: SystemTime::now(),
                active: HashMap::new(),
                recent: VecDeque::new(),
                session_output_buckets: HashMap::new(),
                recent_limit,
            })),
        }
    }

    pub fn publish(&self, event: MonitorEvent) {
        if let Ok(mut store) = self.store.lock() {
            store.apply(event);
        }
    }

    pub fn snapshot(&self) -> MonitorState {
        match self.store.lock() {
            Ok(mut store) => store.snapshot(),
            Err(_) => MonitorState {
                started_at: SystemTime::now(),
                sessions: Vec::new(),
                active: Vec::new(),
                recent: Vec::new(),
            },
        }
    }

    pub fn request_started(
        &self,
        request_id: impl Into<String>,
        session_id: Option<String>,
        session_seq: Option<u64>,
        endpoint: EndpointKind,
    ) {
        self.publish(MonitorEvent::RequestStarted {
            request_id: request_id.into(),
            session_id,
            session_seq,
            endpoint,
        });
    }

    pub fn project_resolved(&self, request_id: impl Into<String>, project: impl Into<String>) {
        self.publish(MonitorEvent::ProjectResolved {
            request_id: request_id.into(),
            project: project.into(),
        });
    }

    pub fn session_sequence_resolved(&self, request_id: impl Into<String>, session_seq: u64) {
        self.publish(MonitorEvent::SessionSequenceResolved {
            request_id: request_id.into(),
            session_seq,
        });
    }

    pub fn provider_selected(
        &self,
        request_id: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        effort: Option<String>,
    ) {
        self.publish(MonitorEvent::ProviderSelected {
            request_id: request_id.into(),
            provider: provider.into(),
            model: model.into(),
            effort,
        });
    }

    pub fn model_resolved(&self, request_id: impl Into<String>, model: impl Into<String>) {
        self.publish(MonitorEvent::ModelResolved {
            request_id: request_id.into(),
            model: model.into(),
        });
    }

    pub fn upstream_started(&self, request_id: impl Into<String>) {
        self.publish(MonitorEvent::UpstreamStarted {
            request_id: request_id.into(),
        });
    }

    pub fn generation_started(&self, request_id: impl Into<String>) {
        self.publish(MonitorEvent::GenerationStarted {
            request_id: request_id.into(),
        });
    }

    pub fn traffic_capture_path(&self, request_id: impl Into<String>, path: PathBuf) {
        self.publish(MonitorEvent::TrafficCapturePath {
            request_id: request_id.into(),
            path,
        });
    }

    pub fn stream_progress(
        &self,
        request_id: impl Into<String>,
        bytes: u64,
        chunks: u64,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.publish(MonitorEvent::StreamProgress {
            request_id: request_id.into(),
            bytes,
            chunks,
            input_tokens,
            output_tokens,
        });
    }

    pub fn usage_updated(
        &self,
        request_id: impl Into<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.publish(MonitorEvent::UsageUpdated {
            request_id: request_id.into(),
            input_tokens,
            output_tokens,
        });
    }

    pub fn request_completed(
        &self,
        request_id: impl Into<String>,
        http_status: u16,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.publish(MonitorEvent::RequestCompleted {
            request_id: request_id.into(),
            http_status,
            input_tokens,
            output_tokens,
        });
    }

    pub fn request_failed(
        &self,
        request_id: impl Into<String>,
        http_status: Option<u16>,
        error: impl Into<String>,
    ) {
        self.publish(MonitorEvent::RequestFailed {
            request_id: request_id.into(),
            http_status,
            error: error.into(),
        });
    }

    pub fn request_abandoned(&self, request_id: impl Into<String>, error: impl Into<String>) {
        self.publish(MonitorEvent::RequestAbandoned {
            request_id: request_id.into(),
            error: error.into(),
        });
    }
}

impl MonitorStore {
    fn apply(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::RequestStarted {
                request_id,
                session_id,
                session_seq,
                endpoint,
            } => {
                self.active.insert(
                    request_id.clone(),
                    ActiveRequest {
                        request_id,
                        session_id,
                        session_seq,
                        project: None,
                        provider: None,
                        model: None,
                        effort: None,
                        endpoint,
                        started_at: SystemTime::now(),
                        started_instant: Instant::now(),
                        generation_started_at: None,
                        generation_started_instant: None,
                        generation_initial_output_tokens: 0,
                        generation_finished_at: None,
                        generation_duration: None,
                        status: RequestStatus::Started,
                        streamed_bytes: 0,
                        stream_chunks: 0,
                        input_tokens: None,
                        output_tokens: None,
                        error: None,
                        traffic_capture_path: None,
                    },
                );
            }
            MonitorEvent::ProjectResolved {
                request_id,
                project,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.project = Some(project);
                }
            }
            MonitorEvent::SessionSequenceResolved {
                request_id,
                session_seq,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.session_seq = Some(session_seq);
                }
            }
            MonitorEvent::ProviderSelected {
                request_id,
                provider,
                model,
                effort,
            } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.provider = Some(provider);
                    active.model = Some(model);
                    active.effort = effort;
                    active.status = RequestStatus::ProviderSelected;
                }
            }
            MonitorEvent::ModelResolved { request_id, model } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.model = Some(match active.model.take() {
                        Some(incoming) if incoming != model => format!("{incoming} → {model}"),
                        Some(incoming) => incoming,
                        None => model,
                    });
                }
            }
            MonitorEvent::UpstreamStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Upstream;
                }
            }
            MonitorEvent::GenerationStarted { request_id } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.generation_started_at = Some(SystemTime::now());
                    active.generation_started_instant = Some(Instant::now());
                    active.generation_initial_output_tokens = active.output_tokens.unwrap_or(0);
                    active.generation_finished_at = None;
                    active.generation_duration = None;
                }
            }
            MonitorEvent::TrafficCapturePath { request_id, path } => {
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.traffic_capture_path = Some(path);
                }
            }
            MonitorEvent::StreamProgress {
                request_id,
                bytes,
                chunks,
                input_tokens,
                output_tokens,
            } => {
                let mut history_update = None;
                if let Some(active) = self.active.get_mut(&request_id) {
                    active.status = RequestStatus::Streaming;
                    if active.generation_started_instant.is_none() {
                        active.generation_started_at = Some(SystemTime::now());
                        active.generation_started_instant = Some(Instant::now());
                        active.generation_initial_output_tokens =
                            output_tokens.or(active.output_tokens).unwrap_or(0);
                    } else {
                        active.generation_finished_at = Some(SystemTime::now());
                        active.generation_duration = active
                            .generation_started_instant
                            .map(|started| started.elapsed());
                    }
                    active.streamed_bytes = active.streamed_bytes.saturating_add(bytes);
                    active.stream_chunks = active.stream_chunks.saturating_add(chunks);
                    active.input_tokens = input_tokens.or(active.input_tokens);
                    active.output_tokens = output_tokens.or(active.output_tokens);
                } else if let Some(completed) = self
                    .recent
                    .iter_mut()
                    .find(|request| request.request_id == request_id)
                {
                    let previous_output_tokens = completed.output_tokens.unwrap_or(0);
                    if let Some(started) = completed.generation_started_instant {
                        completed.generation_finished_at = Some(SystemTime::now());
                        completed.generation_duration = Some(started.elapsed());
                    }
                    completed.streamed_bytes = completed.streamed_bytes.saturating_add(bytes);
                    completed.stream_chunks = completed.stream_chunks.saturating_add(chunks);
                    completed.input_tokens = input_tokens.or(completed.input_tokens);
                    completed.output_tokens = output_tokens.or(completed.output_tokens);
                    let added_tokens = completed
                        .output_tokens
                        .unwrap_or(0)
                        .saturating_sub(previous_output_tokens);
                    if added_tokens > 0 {
                        history_update = Some((
                            completed.session_id.clone(),
                            completed
                                .generation_finished_at
                                .unwrap_or(completed.finished_at),
                            added_tokens,
                        ));
                    }
                }
                if let Some((session_id, timestamp, tokens)) = history_update {
                    self.record_session_output(session_id, timestamp, tokens);
                }
            }
            MonitorEvent::UsageUpdated {
                request_id,
                input_tokens,
                output_tokens,
            } => {
                let mut history_update = None;
                if let Some(active) = self.active.get_mut(&request_id) {
                    if output_tokens.is_some()
                        && let Some(started) = active.generation_started_instant
                    {
                        active.generation_finished_at = Some(SystemTime::now());
                        active.generation_duration = Some(started.elapsed());
                    }
                    active.input_tokens = input_tokens.or(active.input_tokens);
                    active.output_tokens = output_tokens.or(active.output_tokens);
                } else if let Some(completed) = self
                    .recent
                    .iter_mut()
                    .find(|request| request.request_id == request_id)
                {
                    let previous_output_tokens = completed.output_tokens.unwrap_or(0);
                    if output_tokens.is_some()
                        && let Some(started) = completed.generation_started_instant
                    {
                        completed.generation_finished_at = Some(SystemTime::now());
                        completed.generation_duration = Some(started.elapsed());
                    }
                    completed.input_tokens = input_tokens.or(completed.input_tokens);
                    completed.output_tokens = output_tokens.or(completed.output_tokens);
                    let added_tokens = completed
                        .output_tokens
                        .unwrap_or(0)
                        .saturating_sub(previous_output_tokens);
                    if added_tokens > 0 {
                        history_update = Some((
                            completed.session_id.clone(),
                            completed
                                .generation_finished_at
                                .unwrap_or(completed.finished_at),
                            added_tokens,
                        ));
                    }
                }
                if let Some((session_id, timestamp, tokens)) = history_update {
                    self.record_session_output(session_id, timestamp, tokens);
                }
            }
            MonitorEvent::RequestCompleted {
                request_id,
                http_status,
                input_tokens,
                output_tokens,
            } => {
                self.finish(
                    &request_id,
                    RequestStatus::Completed,
                    Some(http_status),
                    input_tokens,
                    output_tokens,
                    None,
                );
            }
            MonitorEvent::RequestFailed {
                request_id,
                http_status,
                error,
            } => {
                self.finish(
                    &request_id,
                    RequestStatus::Failed,
                    http_status,
                    None,
                    None,
                    Some(error),
                );
            }
            MonitorEvent::RequestAbandoned { request_id, error } => {
                self.finish_active(
                    &request_id,
                    RequestStatus::Failed,
                    None,
                    None,
                    None,
                    Some(error),
                );
            }
        }
    }

    fn finish_active(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error: Option<String>,
    ) {
        if self.active.contains_key(request_id) {
            self.finish(
                request_id,
                status,
                http_status,
                input_tokens,
                output_tokens,
                error,
            );
        }
    }

    fn finish(
        &mut self,
        request_id: &str,
        status: RequestStatus,
        http_status: Option<u16>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        error: Option<String>,
    ) {
        let mut active = self
            .active
            .remove(request_id)
            .unwrap_or_else(|| ActiveRequest {
                request_id: request_id.to_string(),
                session_id: None,
                session_seq: None,
                project: None,
                provider: None,
                model: None,
                effort: None,
                endpoint: EndpointKind::Messages,
                started_at: SystemTime::now(),
                started_instant: Instant::now(),
                generation_started_at: None,
                generation_started_instant: None,
                generation_initial_output_tokens: 0,
                generation_finished_at: None,
                generation_duration: None,
                status: RequestStatus::Started,
                streamed_bytes: 0,
                stream_chunks: 0,
                input_tokens: None,
                output_tokens: None,
                error: None,
                traffic_capture_path: None,
            });
        if output_tokens.is_some()
            && let Some(started) = active.generation_started_instant
        {
            active.generation_finished_at = Some(SystemTime::now());
            active.generation_duration = Some(started.elapsed());
        }
        let completed = CompletedRequest {
            request_id: active.request_id,
            session_id: active.session_id,
            session_seq: active.session_seq,
            project: active.project,
            provider: active.provider,
            model: active.model,
            effort: active.effort,
            endpoint: active.endpoint,
            started_at: active.started_at,
            finished_at: SystemTime::now(),
            generation_started_at: active.generation_started_at,
            generation_started_instant: active.generation_started_instant,
            generation_initial_output_tokens: active.generation_initial_output_tokens,
            generation_finished_at: active.generation_finished_at,
            generation_duration: active.generation_duration,
            status,
            http_status,
            latency: active.started_instant.elapsed(),
            streamed_bytes: active.streamed_bytes,
            stream_chunks: active.stream_chunks,
            input_tokens: input_tokens.or(active.input_tokens),
            output_tokens: output_tokens.or(active.output_tokens),
            error: error.or(active.error),
            traffic_capture_path: active.traffic_capture_path,
        };
        let history_update = completed
            .output_tokens
            .filter(|tokens| *tokens > 0)
            .map(|tokens| {
                (
                    completed.session_id.clone(),
                    completed
                        .generation_finished_at
                        .unwrap_or(completed.finished_at),
                    tokens,
                )
            });
        self.recent.push_front(completed);
        while self.recent.len() > self.recent_limit {
            self.recent.pop_back();
        }
        if let Some((session_id, timestamp, tokens)) = history_update {
            self.record_session_output(session_id, timestamp, tokens);
        }
    }

    fn record_session_output(
        &mut self,
        session_id: Option<String>,
        timestamp: SystemTime,
        tokens: u64,
    ) {
        let bucket = session_token_bucket(timestamp);
        let buckets = self.session_output_buckets.entry(session_id).or_default();
        match buckets.binary_search_by_key(&bucket, |(bucket, _)| *bucket) {
            Ok(index) => buckets[index].1 = buckets[index].1.saturating_add(tokens),
            Err(index) => buckets.insert(index, (bucket, tokens)),
        }
        let excess_buckets = buckets
            .len()
            .saturating_sub(SESSION_TOKEN_HISTORY_BUCKET_LIMIT);
        if excess_buckets > 0 {
            buckets.drain(..excess_buckets);
        }
        self.enforce_session_history_limit();
    }

    fn enforce_session_history_limit(&mut self) {
        if self.session_output_buckets.len() <= SESSION_TOKEN_HISTORY_SESSION_LIMIT {
            return;
        }

        let available: HashSet<_> = self.session_output_buckets.keys().cloned().collect();
        let mut keep = HashSet::with_capacity(SESSION_TOKEN_HISTORY_SESSION_LIMIT);

        // Active sessions take precedence. Sorting makes eviction deterministic when the
        // number of active sessions alone exceeds the hard limit.
        let mut active_sessions: Vec<_> = self
            .active
            .values()
            .map(|request| request.session_id.clone())
            .filter(|session_id| available.contains(session_id))
            .collect();
        active_sessions.sort();
        active_sessions.dedup();
        for session_id in active_sessions {
            if keep.len() == SESSION_TOKEN_HISTORY_SESSION_LIMIT {
                break;
            }
            keep.insert(session_id);
        }

        // `recent` is newest-first, so recently completed sessions are retained next.
        for request in &self.recent {
            if keep.len() == SESSION_TOKEN_HISTORY_SESSION_LIMIT {
                break;
            }
            if available.contains(&request.session_id) {
                keep.insert(request.session_id.clone());
            }
        }

        // Fill any remaining capacity with the histories that have the newest token bucket.
        let mut remaining: Vec<_> = self
            .session_output_buckets
            .iter()
            .filter(|(session_id, _)| !keep.contains(*session_id))
            .map(|(session_id, buckets)| {
                (
                    session_id.clone(),
                    buckets.last().map(|(bucket, _)| *bucket).unwrap_or(0),
                )
            })
            .collect();
        remaining.sort_by(|(left_id, left_bucket), (right_id, right_bucket)| {
            right_bucket
                .cmp(left_bucket)
                .then_with(|| left_id.cmp(right_id))
        });
        for (session_id, _) in remaining {
            if keep.len() == SESSION_TOKEN_HISTORY_SESSION_LIMIT {
                break;
            }
            keep.insert(session_id);
        }

        self.session_output_buckets
            .retain(|session_id, _| keep.contains(session_id));
    }

    fn prune_session_output_window(&mut self, reference_bucket: u64) {
        let oldest_bucket = reference_bucket
            .saturating_sub(SESSION_TOKEN_HISTORY_BUCKET_LIMIT.saturating_sub(1) as u64);
        for buckets in self.session_output_buckets.values_mut() {
            let first_retained = buckets.partition_point(|(bucket, _)| *bucket < oldest_bucket);
            let after_last_retained =
                buckets.partition_point(|(bucket, _)| *bucket <= reference_bucket);
            buckets.truncate(after_last_retained);
            buckets.drain(..first_retained);
        }
        self.session_output_buckets
            .retain(|_, buckets| !buckets.is_empty());
    }

    fn snapshot(&mut self) -> MonitorState {
        self.prune_session_output_window(session_token_bucket(SystemTime::now()));
        let mut active: Vec<_> = self.active.values().cloned().collect();
        active.sort_by_key(|request| request.started_at);
        let sessions = session_summaries(&active, &self.recent, &self.session_output_buckets);
        MonitorState {
            started_at: self.started_at,
            sessions,
            active,
            recent: self.recent.iter().cloned().collect(),
        }
    }
}

fn session_summaries(
    active: &[ActiveRequest],
    recent: &VecDeque<CompletedRequest>,
    session_output_buckets: &HashMap<Option<String>, Vec<(u64, u64)>>,
) -> Vec<SessionSummary> {
    let mut sessions: HashMap<Option<String>, SessionSummary> = HashMap::new();
    for request in recent.iter().rev() {
        let entry = sessions
            .entry(request.session_id.clone())
            .or_insert_with(|| SessionSummary {
                session_id: request.session_id.clone(),
                project: request.project.clone(),
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                provider: None,
                model: None,
                effort: None,
                last_seen: request.finished_at,
                input_tokens: 0,
                output_tokens: 0,
                output_token_samples: Vec::new(),
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-".to_string(),
            });
        entry.request_count += 1;
        if request.status == RequestStatus::Failed {
            entry.failure_count += 1;
        }
        if let Some(project) = &request.project {
            entry.project = Some(project.clone());
        }
        if let Some(provider) = &request.provider {
            entry.provider = Some(provider.clone());
        }
        if let Some(model) = &request.model {
            entry.model = Some(model.clone());
        }
        if let Some(effort) = &request.effort {
            entry.effort = Some(effort.clone());
        }
        entry.last_seen = max_system_time(entry.last_seen, request.finished_at);
        entry.input_tokens = entry
            .input_tokens
            .saturating_add(request.input_tokens.unwrap_or(0));
        entry.output_tokens = entry
            .output_tokens
            .saturating_add(request.output_tokens.unwrap_or(0));
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label().to_string();
    }

    for request in active {
        let entry = sessions
            .entry(request.session_id.clone())
            .or_insert_with(|| SessionSummary {
                session_id: request.session_id.clone(),
                project: request.project.clone(),
                active_count: 0,
                request_count: 0,
                failure_count: 0,
                provider: None,
                model: None,
                effort: None,
                last_seen: request.started_at,
                input_tokens: 0,
                output_tokens: 0,
                output_token_samples: Vec::new(),
                rate_output_tokens: 0,
                generation_duration: Duration::ZERO,
                last_status: "-".to_string(),
            });
        entry.active_count += 1;
        entry.request_count += 1;
        if let Some(project) = &request.project {
            entry.project = Some(project.clone());
        }
        if let Some(provider) = &request.provider {
            entry.provider = Some(provider.clone());
        }
        if let Some(model) = &request.model {
            entry.model = Some(model.clone());
        }
        if let Some(effort) = &request.effort {
            entry.effort = Some(effort.clone());
        }
        entry.last_seen = max_system_time(entry.last_seen, request.started_at);
        entry.input_tokens = entry
            .input_tokens
            .saturating_add(request.input_tokens.unwrap_or(0));
        entry.output_tokens = entry
            .output_tokens
            .saturating_add(request.output_tokens.unwrap_or(0));
        if let (Some(tokens), Some(duration)) = (
            request
                .output_tokens
                .and_then(|tokens| tokens.checked_sub(request.generation_initial_output_tokens))
                .filter(|tokens| *tokens > 0),
            request
                .generation_duration
                .filter(|duration| !duration.is_zero()),
        ) {
            entry.rate_output_tokens = entry.rate_output_tokens.saturating_add(tokens);
            entry.generation_duration = entry.generation_duration.saturating_add(duration);
        }
        entry.last_status = request.status.label().to_string();
    }

    for (session_id, session) in &mut sessions {
        if let Some(buckets) = session_output_buckets.get(session_id) {
            session.output_token_samples = buckets
                .iter()
                .map(|(bucket, tokens)| (session_token_bucket_start(*bucket), *tokens))
                .collect();
        }
    }

    let mut out: Vec<_> = sessions.into_values().collect();
    out.sort_by_key(SessionSummary::label);
    out
}

fn session_token_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

fn session_token_bucket_start(bucket: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(bucket.saturating_mul(SESSION_TOKEN_BUCKET_SECS))
}

fn max_system_time(left: SystemTime, right: SystemTime) -> SystemTime {
    if right.duration_since(left).is_ok() {
        right
    } else {
        left
    }
}

pub fn throughput(
    output_tokens: Option<u64>,
    streamed_bytes: u64,
    stream_chunks: u64,
    elapsed: Duration,
) -> Throughput {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return Throughput::None;
    }
    if let Some(tokens) = output_tokens.filter(|tokens| *tokens > 0) {
        return Throughput::TokensPerSecond(tokens as f64 / secs);
    }
    if streamed_bytes > 0 {
        return Throughput::BytesPerSecond(streamed_bytes as f64 / secs);
    }
    if stream_chunks > 0 {
        return Throughput::EventsPerSecond(stream_chunks as f64 / secs);
    }
    Throughput::None
}

pub fn anthropic_total_input_tokens(usage: &serde_json::Value) -> Option<u64> {
    let uncached = usage.get("input_tokens").and_then(|value| value.as_u64());
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|value| value.as_u64());
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|value| value.as_u64());

    if uncached.is_none() && cache_creation.is_none() && cache_read.is_none() {
        return None;
    }

    Some(
        uncached
            .unwrap_or(0)
            .saturating_add(cache_creation.unwrap_or(0))
            .saturating_add(cache_read.unwrap_or(0)),
    )
}

pub fn usage_from_anthropic_sse(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
    let text = String::from_utf8_lossy(bytes);
    let mut input_tokens = None;
    let mut output_tokens = None;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
            continue;
        };
        for usage in [
            value.pointer("/usage"),
            value.pointer("/delta/usage"),
            value.pointer("/message/usage"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(tokens) = anthropic_total_input_tokens(usage) {
                input_tokens = Some(tokens);
            }
            if let Some(tokens) = usage.get("output_tokens").and_then(|value| value.as_u64()) {
                output_tokens = Some(tokens);
            }
        }
    }
    (input_tokens, output_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_requests_appear_active() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("s1".to_string()),
            Some(3),
            EndpointKind::Messages,
        );
        let state = monitor.snapshot();
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].request_id, "r1");
        assert_eq!(state.active[0].session_id.as_deref(), Some("s1"));
        assert_eq!(state.active[0].session_seq, Some(3));
    }

    #[test]
    fn resolved_model_appends_to_incoming_alias() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "claude-sonnet-4-6", None);
        monitor.model_resolved("r1", "gpt-5.4");

        let state = monitor.snapshot();
        assert_eq!(
            state.active[0].model.as_deref(),
            Some("claude-sonnet-4-6 → gpt-5.4")
        );
    }

    #[test]
    fn identical_resolved_model_is_shown_once() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.6-sol", None);
        monitor.model_resolved("r1", "gpt-5.6-sol");

        let state = monitor.snapshot();
        assert_eq!(state.active[0].model.as_deref(), Some("gpt-5.6-sol"));
    }

    #[test]
    fn generation_baseline_pairs_total_usage_with_the_full_observed_interval() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.generation_started("r1");
        monitor.stream_progress("r1", 50, 1, Some(1_225), Some(141));
        monitor.request_completed("r1", 200, None, None);

        let request = &monitor.snapshot().recent[0];

        assert!(
            request
                .generation_duration
                .is_some_and(|duration| !duration.is_zero())
        );
        assert!(matches!(request.rate(), Throughput::TokensPerSecond(_)));
    }

    #[test]
    fn first_stream_progress_has_no_rate_without_an_interval() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.stream_progress("r1", 50, 1, Some(1_225), Some(141));

        let state = monitor.snapshot();
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].rate(), Throughput::None);
    }

    #[test]
    fn late_stream_progress_extends_stream_timing_without_extending_request_latency() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.stream_progress("r1", 100, 1, Some(0), Some(0));
        monitor.request_completed("r1", 200, None, None);
        let completed = monitor.snapshot().recent[0].clone();
        monitor.stream_progress("r1", 50, 1, Some(1_225), Some(141));

        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent[0].streamed_bytes, 150);
        assert_eq!(state.recent[0].stream_chunks, 2);
        assert_eq!(state.recent[0].input_tokens, Some(1_225));
        assert_eq!(state.recent[0].output_tokens, Some(141));
        assert_eq!(state.recent[0].finished_at, completed.finished_at);
        assert_eq!(state.recent[0].latency, completed.latency);
        assert!(state.recent[0].generation_duration > completed.generation_duration);
        assert!(matches!(
            state.recent[0].rate(),
            Throughput::TokensPerSecond(_)
        ));
        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .sum::<u64>(),
            141
        );
    }

    #[test]
    fn completed_requests_leave_active_and_enter_recent() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.provider_selected("r1", "codex", "gpt-5.5", Some("high".to_string()));
        monitor.request_completed("r1", 200, Some(10), Some(20));
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].provider.as_deref(), Some("codex"));
        assert_eq!(state.recent[0].effort.as_deref(), Some("high"));
        assert_eq!(state.recent[0].output_tokens, Some(20));
    }

    #[test]
    fn failed_requests_preserve_error_summary() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.request_failed("r1", Some(400), "Unknown model");
        let state = monitor.snapshot();
        assert_eq!(state.recent[0].status, RequestStatus::Failed);
        assert_eq!(state.recent[0].http_status, Some(400));
        assert_eq!(state.recent[0].error.as_deref(), Some("Unknown model"));
    }

    #[test]
    fn abandoned_requests_leave_active_once() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.request_abandoned("r1", "request dropped");
        monitor.request_abandoned("r1", "request dropped again");
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].status, RequestStatus::Failed);
        assert_eq!(state.recent[0].http_status, None);
        assert_eq!(state.recent[0].error.as_deref(), Some("request dropped"));
    }

    #[test]
    fn completed_requests_ignore_late_abandonment() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("r1", None, None, EndpointKind::Messages);
        monitor.request_completed("r1", 200, None, None);
        monitor.request_abandoned("r1", "request dropped");
        let state = monitor.snapshot();
        assert!(state.active.is_empty());
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].status, RequestStatus::Completed);
    }

    #[test]
    fn bounded_recent_history_drops_oldest() {
        let monitor = MonitorHandle::new(2);
        for id in ["r1", "r2", "r3"] {
            monitor.request_started(id, None, None, EndpointKind::Messages);
            monitor.request_completed(id, 200, None, None);
        }
        let state = monitor.snapshot();
        let ids: Vec<_> = state
            .recent
            .iter()
            .map(|request| request.request_id.as_str())
            .collect();
        assert_eq!(ids, vec!["r3", "r2"]);
    }

    #[test]
    fn throughput_selects_best_available_signal() {
        let elapsed = Duration::from_secs(2);
        assert_eq!(
            throughput(Some(84), 1024, 10, elapsed),
            Throughput::TokensPerSecond(42.0)
        );
        assert_eq!(
            throughput(None, 2048, 10, elapsed),
            Throughput::BytesPerSecond(1024.0)
        );
        assert_eq!(
            throughput(None, 0, 36, elapsed),
            Throughput::EventsPerSecond(18.0)
        );
    }

    #[test]
    fn sse_usage_extracts_final_message_delta_tokens() {
        let sse = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":12,"output_tokens":48}}

"#;
        assert_eq!(usage_from_anthropic_sse(sse), (Some(12), Some(48)));
    }

    #[test]
    fn sse_usage_includes_cache_creation_and_read_tokens() {
        let sse = br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":298,"cache_creation_input_tokens":1024,"cache_read_input_tokens":8192,"output_tokens":3}}}

"#;
        assert_eq!(usage_from_anthropic_sse(sse), (Some(9_514), Some(3)));
    }

    #[test]
    fn anthropic_total_input_tokens_uses_every_input_bucket_and_saturates() {
        assert_eq!(
            anthropic_total_input_tokens(&serde_json::json!({
                "input_tokens": 298,
                "cache_creation_input_tokens": 1_024,
                "cache_read_input_tokens": 8_192,
            })),
            Some(9_514)
        );
        assert_eq!(
            anthropic_total_input_tokens(&serde_json::json!({
                "input_tokens": u64::MAX,
                "cache_creation_input_tokens": 1,
                "cache_read_input_tokens": 1,
            })),
            Some(u64::MAX)
        );
        assert_eq!(
            anthropic_total_input_tokens(&serde_json::json!({"output_tokens": 3})),
            None
        );
    }

    fn completed_request(
        request_id: &str,
        session_id: &str,
        output_tokens: u64,
        latency: Duration,
        generation_duration: Option<Duration>,
    ) -> CompletedRequest {
        CompletedRequest {
            request_id: request_id.to_string(),
            session_id: Some(session_id.to_string()),
            session_seq: None,
            project: None,
            provider: Some("codex".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            effort: None,
            endpoint: EndpointKind::Messages,
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: SystemTime::UNIX_EPOCH + latency,
            generation_started_at: generation_duration.map(|_| SystemTime::UNIX_EPOCH),
            generation_started_instant: None,
            generation_initial_output_tokens: 0,
            generation_finished_at: generation_duration
                .map(|duration| SystemTime::UNIX_EPOCH + duration),
            generation_duration,
            status: RequestStatus::Completed,
            http_status: Some(200),
            latency,
            streamed_bytes: 0,
            stream_chunks: 0,
            input_tokens: None,
            output_tokens: Some(output_tokens),
            error: None,
            traffic_capture_path: None,
        }
    }

    #[test]
    fn completed_request_rate_uses_stream_interval_instead_of_request_latency() {
        let request = completed_request(
            "r1",
            "s1",
            120,
            Duration::from_secs(30),
            Some(Duration::from_secs(4)),
        );

        assert_eq!(request.rate(), Throughput::TokensPerSecond(30.0));
    }

    #[test]
    fn request_rate_uses_token_delta_from_the_initial_observation() {
        let mut request = completed_request(
            "r1",
            "s1",
            120,
            Duration::from_secs(30),
            Some(Duration::from_secs(4)),
        );
        request.generation_initial_output_tokens = 20;

        assert_eq!(request.rate(), Throughput::TokensPerSecond(25.0));
    }

    #[test]
    fn session_rate_combines_request_tokens_and_generation_intervals() {
        let recent = VecDeque::from([
            completed_request(
                "r2",
                "s1",
                50,
                Duration::from_secs(40),
                Some(Duration::from_secs(1)),
            ),
            completed_request(
                "r1",
                "s1",
                100,
                Duration::from_secs(20),
                Some(Duration::from_secs(4)),
            ),
        ]);

        let sessions = session_summaries(&[], &recent, &HashMap::new());

        assert_eq!(sessions[0].output_tokens, 150);
        assert_eq!(sessions[0].generation_duration, Duration::from_secs(5));
        assert_eq!(sessions[0].rate(), Throughput::TokensPerSecond(30.0));
    }

    #[test]
    fn output_without_observed_stream_interval_has_no_output_rate() {
        let request = completed_request("r1", "s1", 120, Duration::from_secs(30), None);
        let recent = VecDeque::from([request.clone()]);

        assert_eq!(request.rate(), Throughput::None);
        assert_eq!(
            session_summaries(&[], &recent, &HashMap::new())[0].rate(),
            Throughput::None
        );
    }

    #[test]
    fn session_rate_excludes_interval_without_output_usage() {
        let mut tokenless = completed_request(
            "tokenless",
            "s1",
            0,
            Duration::from_secs(30),
            Some(Duration::from_secs(100)),
        );
        tokenless.output_tokens = None;
        let recent = VecDeque::from([
            tokenless,
            completed_request(
                "measured",
                "s1",
                100,
                Duration::from_secs(20),
                Some(Duration::from_secs(4)),
            ),
        ]);

        assert_eq!(
            session_summaries(&[], &recent, &HashMap::new())[0].rate(),
            Throughput::TokensPerSecond(25.0)
        );
    }

    #[test]
    fn session_rate_excludes_output_without_a_matching_stream_interval() {
        let recent = VecDeque::from([
            completed_request("buffered", "s1", 900, Duration::from_secs(30), None),
            completed_request(
                "streamed",
                "s1",
                100,
                Duration::from_secs(20),
                Some(Duration::from_secs(4)),
            ),
        ]);

        let session = &session_summaries(&[], &recent, &HashMap::new())[0];

        assert_eq!(session.output_tokens, 1_000);
        assert_eq!(session.rate(), Throughput::TokensPerSecond(25.0));
    }

    #[test]
    fn session_summaries_group_recent_and_active_requests() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("s1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.project_resolved("r1", "example");
        monitor.provider_selected("r1", "codex", "gpt-5.5", None);
        monitor.request_completed("r1", 200, Some(10), Some(20));
        monitor.request_started(
            "r2",
            Some("s1".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        monitor.provider_selected("r2", "codex", "gpt-5.5", Some("xhigh".to_string()));
        let state = monitor.snapshot();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].label(), "s1");
        assert_eq!(state.sessions[0].project.as_deref(), Some("example"));
        assert_eq!(state.sessions[0].request_count, 2);
        assert_eq!(state.sessions[0].active_count, 1);
        assert_eq!(state.sessions[0].effort.as_deref(), Some("xhigh"));
        assert_eq!(state.sessions[0].output_tokens, 20);
        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .collect::<Vec<_>>(),
            vec![20]
        );
    }

    #[test]
    fn session_output_history_survives_request_eviction() {
        let monitor = MonitorHandle::new(1);
        for (request_id, tokens) in [("oldest", 20), ("newest", 80)] {
            monitor.request_started(
                request_id,
                Some("s1".to_string()),
                None,
                EndpointKind::Messages,
            );
            monitor.request_completed(request_id, 200, None, Some(tokens));
        }

        let state = monitor.snapshot();

        assert_eq!(state.recent.len(), 1);
        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .sum::<u64>(),
            100
        );
    }

    #[test]
    fn session_output_history_stays_bounded_after_multiple_days() {
        let monitor = MonitorHandle::new(1);
        let start = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        let bucket_count = 2 * 24 * 60 * 60 / SESSION_TOKEN_BUCKET_SECS;

        let mut store = monitor.store.lock().expect("monitor store");
        for offset in 0..=bucket_count {
            store.record_session_output(
                Some("long-running".to_string()),
                start + Duration::from_secs(offset * SESSION_TOKEN_BUCKET_SECS),
                1,
            );
        }

        let buckets = &store.session_output_buckets[&Some("long-running".to_string())];
        assert_eq!(buckets.len(), SESSION_TOKEN_HISTORY_BUCKET_LIMIT);
        assert_eq!(
            buckets.last().expect("latest bucket").0 - buckets[0].0 + 1,
            SESSION_TOKEN_HISTORY_BUCKET_LIMIT as u64
        );
    }

    #[test]
    fn session_output_history_caps_high_cardinality() {
        let monitor = MonitorHandle::new(1);
        let timestamp = SystemTime::now();
        let mut store = monitor.store.lock().expect("monitor store");

        for index in 0..(SESSION_TOKEN_HISTORY_SESSION_LIMIT + 100) {
            store.record_session_output(Some(format!("session-{index:04}")), timestamp, 1);
        }

        assert_eq!(
            store.session_output_buckets.len(),
            SESSION_TOKEN_HISTORY_SESSION_LIMIT
        );
    }

    #[test]
    fn active_and_recent_session_histories_survive_cardinality_eviction() {
        let monitor = MonitorHandle::new(1);
        let recent_session = Some("zz-recent".to_string());
        let active_session = Some("zz-active".to_string());

        monitor.request_started(
            "recent-request",
            recent_session.clone(),
            None,
            EndpointKind::Messages,
        );
        monitor.request_completed("recent-request", 200, None, Some(1));
        monitor.request_started(
            "active-request",
            active_session.clone(),
            None,
            EndpointKind::Messages,
        );

        let mut store = monitor.store.lock().expect("monitor store");
        store.record_session_output(active_session.clone(), SystemTime::now(), 1);
        for index in 0..(SESSION_TOKEN_HISTORY_SESSION_LIMIT + 100) {
            store.record_session_output(
                Some(format!("untracked-{index:04}")),
                SystemTime::now(),
                1,
            );
        }

        assert_eq!(
            store.session_output_buckets.len(),
            SESSION_TOKEN_HISTORY_SESSION_LIMIT
        );
        assert!(store.session_output_buckets.contains_key(&active_session));
        assert!(store.session_output_buckets.contains_key(&recent_session));
    }

    #[test]
    fn snapshot_excludes_token_buckets_outside_the_history_window() {
        let monitor = MonitorHandle::new(1);
        let session_id = Some("visible-session".to_string());
        monitor.request_started(
            "active-request",
            session_id.clone(),
            None,
            EndpointKind::Messages,
        );
        let current_bucket = session_token_bucket(SystemTime::now());
        monitor
            .store
            .lock()
            .expect("monitor store")
            .session_output_buckets
            .insert(
                session_id,
                vec![
                    (current_bucket.saturating_sub(500), 10),
                    (current_bucket, 20),
                ],
            );

        let state = monitor.snapshot();

        assert_eq!(
            state.sessions[0]
                .output_token_samples
                .iter()
                .map(|(_, tokens)| *tokens)
                .collect::<Vec<_>>(),
            vec![20]
        );
        assert_eq!(
            monitor
                .store
                .lock()
                .expect("monitor store")
                .session_output_buckets
                .values()
                .next()
                .expect("retained history")
                .len(),
            1
        );
    }

    #[test]
    fn session_order_is_stable_across_activity() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "r1",
            Some("session-b".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.request_started(
            "r2",
            Some("session-a".to_string()),
            Some(1),
            EndpointKind::Messages,
        );

        let first: Vec<_> = monitor
            .snapshot()
            .sessions
            .iter()
            .map(SessionSummary::label)
            .collect();
        monitor.request_completed("r1", 200, None, None);
        monitor.request_started(
            "r3",
            Some("session-b".to_string()),
            Some(2),
            EndpointKind::Messages,
        );
        let second: Vec<_> = monitor
            .snapshot()
            .sessions
            .iter()
            .map(SessionSummary::label)
            .collect();

        assert_eq!(first, vec!["session-a", "session-b"]);
        assert_eq!(second, first);
    }
}
