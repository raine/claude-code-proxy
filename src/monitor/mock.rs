use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use super::{
    ActiveRequest, CompletedRequest, EndpointKind, MonitorState, RequestStatus, session_summaries,
};

const TICK_MILLIS: u64 = 250;
const REQUEST_TICKS: u64 = 24;
const COMPLETED_PHASE: u64 = 20;

#[derive(Debug)]
pub struct MockMonitor {
    started_at: SystemTime,
    output_buckets: HashMap<Option<String>, Vec<(u64, u64)>>,
    tick: u64,
}

impl MockMonitor {
    pub fn new() -> Self {
        let now = SystemTime::now();
        Self {
            started_at: now - Duration::from_secs(3_723),
            output_buckets: initial_output_buckets(now),
            tick: 0,
        }
    }

    pub fn snapshot(&mut self) -> MonitorState {
        let now = SystemTime::now();
        advance_output_buckets(&mut self.output_buckets, now, self.tick);
        let state = mock_state_for_tick(
            self.started_at,
            now,
            Instant::now(),
            self.tick,
            &self.output_buckets,
        );
        self.tick = self.tick.wrapping_add(1);
        state
    }
}

impl Default for MockMonitor {
    fn default() -> Self {
        Self::new()
    }
}

pub fn mock_state() -> MonitorState {
    let now = SystemTime::now();
    let mut output_buckets = initial_output_buckets(now);
    advance_output_buckets(&mut output_buckets, now, 0);
    mock_state_for_tick(
        now - Duration::from_secs(3_723),
        now,
        Instant::now(),
        0,
        &output_buckets,
    )
}

fn mock_state_for_tick(
    started_at: SystemTime,
    now: SystemTime,
    instant_now: Instant,
    tick: u64,
    output_buckets: &HashMap<Option<String>, Vec<(u64, u64)>>,
) -> MonitorState {
    let mut streaming = active_request(
        now,
        instant_now,
        "req-active-codex",
        Some("57c7c914-ada4-4f40-9672-985f950fbb66"),
        Some(12),
        EndpointKind::Messages,
        Duration::from_secs(14),
        RequestStatus::Streaming,
    );
    streaming.project = Some("claude-code-proxy".to_string());
    streaming.provider = Some("codex".to_string());
    streaming.model = Some("claude-sonnet-4-6 → gpt-5.6-sol".to_string());
    streaming.effort = Some("high".to_string());
    streaming.generation_started_at = Some(now - Duration::from_secs(10));
    streaming.generation_started_instant = Some(instant_now - Duration::from_secs(10));
    streaming.generation_initial_output_tokens = 20;
    streaming.generation_finished_at = Some(now - Duration::from_secs(2));
    streaming.generation_duration = Some(Duration::from_secs(8));
    streaming.streamed_bytes = 18_432;
    streaming.stream_chunks = 96;
    streaming.input_tokens = Some(12_480);
    streaming.output_tokens = Some(420);
    streaming.traffic_capture_path = Some(PathBuf::from(
        "/tmp/claude-code-proxy-demo/traffic/req-active-codex",
    ));
    let simulated_elapsed = Duration::from_millis(tick.saturating_mul(TICK_MILLIS));
    streaming.started_at = now - Duration::from_secs(14) - simulated_elapsed;
    streaming.started_instant = instant_now - Duration::from_secs(14) - simulated_elapsed;
    streaming.generation_started_at = Some(now - Duration::from_secs(10) - simulated_elapsed);
    streaming.generation_started_instant =
        Some(instant_now - Duration::from_secs(10) - simulated_elapsed);
    streaming.output_tokens = Some(420_u64.saturating_add(simulated_output_tokens(tick)));
    streaming.input_tokens = Some(12_480_u64.saturating_add(tick / 4));
    streaming.streamed_bytes = 18_432_u64.saturating_add(tick.saturating_mul(384));
    streaming.stream_chunks = 96_u64.saturating_add(tick.saturating_mul(3));
    streaming.generation_duration = Some(Duration::from_secs(4));
    streaming.generation_initial_output_tokens = streaming
        .output_tokens
        .unwrap_or(0)
        .saturating_sub(160 + (tick % 12) * 9);

    let mut upstream = active_request(
        now,
        instant_now,
        "req-active-kimi",
        Some("terminal-refactor"),
        Some(4),
        EndpointKind::Messages,
        Duration::from_secs(6),
        RequestStatus::Upstream,
    );
    upstream.project = Some("terminal-dashboard".to_string());
    upstream.provider = Some("kimi".to_string());
    upstream.model = Some("kimi-k2.6".to_string());
    upstream.effort = Some("medium".to_string());
    upstream.input_tokens = Some(3_200);

    let mut selected = active_request(
        now,
        instant_now,
        "req-active-grok",
        Some("mobile-client"),
        Some(9),
        EndpointKind::Messages,
        Duration::from_secs(3),
        RequestStatus::ProviderSelected,
    );
    selected.project = Some("companion-app".to_string());
    selected.provider = Some("grok".to_string());
    selected.model = Some("grok-composer-2.5-fast".to_string());
    selected.effort = Some("low".to_string());

    let started = active_request(
        now,
        instant_now,
        "req-active-count",
        None,
        None,
        EndpointKind::CountTokens,
        Duration::from_secs(1),
        RequestStatus::Started,
    );

    let mut byte_stream = active_request(
        now,
        instant_now,
        "req-active-cursor",
        Some("cursor-session"),
        Some(2),
        EndpointKind::Messages,
        Duration::from_secs(22),
        RequestStatus::Streaming,
    );
    byte_stream.project = Some("responsive-layout-lab".to_string());
    byte_stream.provider = Some("cursor".to_string());
    byte_stream.model = Some("cursor:claude-4.6-opus-high-thinking".to_string());
    byte_stream.generation_started_at = Some(now - Duration::from_secs(20));
    byte_stream.generation_started_instant = Some(instant_now - Duration::from_secs(20));
    byte_stream.generation_finished_at = Some(now - Duration::from_secs(4));
    byte_stream.streamed_bytes = 32_768_u64.saturating_add(tick.saturating_mul(640));
    byte_stream.stream_chunks = 128_u64.saturating_add(tick.saturating_mul(4));
    byte_stream.generation_duration = Some(Duration::from_millis(8_000 + (tick % 16) * 500));

    let mut active = vec![streaming, upstream, selected, started, byte_stream];
    let mut recent = VecDeque::new();

    let mut success = completed_request(
        now,
        "req-complete-codex",
        Some("57c7c914-ada4-4f40-9672-985f950fbb66"),
        Some(11),
        EndpointKind::Messages,
        Duration::from_secs(18),
        Duration::from_millis(4_820),
        RequestStatus::Completed,
        Some(200),
    );
    success.project = Some("claude-code-proxy".to_string());
    success.provider = Some("codex".to_string());
    success.model = Some("claude-sonnet-4-6 → gpt-5.6-terra".to_string());
    success.effort = Some("xhigh".to_string());
    success.generation_duration = Some(Duration::from_secs(4));
    success.generation_initial_output_tokens = 32;
    success.streamed_bytes = 24_576;
    success.stream_chunks = 142;
    success.input_tokens = Some(125_600);
    success.output_tokens = Some(832);
    success.traffic_capture_path = Some(PathBuf::from(
        "/tmp/claude-code-proxy-demo/traffic/req-complete-codex",
    ));
    recent.push_back(success);

    let mut unavailable = completed_request(
        now,
        "req-failed-kimi",
        Some("terminal-refactor"),
        Some(3),
        EndpointKind::Messages,
        Duration::from_secs(41),
        Duration::from_millis(735),
        RequestStatus::Failed,
        Some(502),
    );
    unavailable.project = Some("terminal-dashboard".to_string());
    unavailable.provider = Some("kimi".to_string());
    unavailable.model = Some("kimi-for-coding".to_string());
    unavailable.effort = Some("high".to_string());
    unavailable.input_tokens = Some(8_900);
    unavailable.error = Some("upstream connection closed before response headers".to_string());
    unavailable.traffic_capture_path = Some(PathBuf::from(
        "/tmp/claude-code-proxy-demo/errors/req-failed-kimi.json",
    ));
    recent.push_back(unavailable);

    let mut rate_limited = completed_request(
        now,
        "req-failed-cursor",
        Some("cursor-session"),
        Some(1),
        EndpointKind::Messages,
        Duration::from_secs(66),
        Duration::from_secs(2),
        RequestStatus::Failed,
        Some(429),
    );
    rate_limited.project = Some("responsive-layout-lab".to_string());
    rate_limited.provider = Some("cursor".to_string());
    rate_limited.model = Some("cursor:claude-4.6-opus-high-thinking".to_string());
    rate_limited.error = Some("provider rate limit reached; retry after 30 seconds".to_string());
    recent.push_back(rate_limited);

    let mut bytes = completed_request(
        now,
        "req-complete-grok",
        Some("mobile-client"),
        Some(8),
        EndpointKind::Messages,
        Duration::from_secs(93),
        Duration::from_secs(3),
        RequestStatus::Completed,
        Some(201),
    );
    bytes.project = Some("companion-app".to_string());
    bytes.provider = Some("grok".to_string());
    bytes.model = Some("grok-4.5".to_string());
    bytes.generation_duration = Some(Duration::from_secs(2));
    bytes.streamed_bytes = 8_192;
    bytes.stream_chunks = 64;
    recent.push_back(bytes);

    let mut events = completed_request(
        now,
        "req-complete-events",
        Some("event-stream"),
        Some(5),
        EndpointKind::Messages,
        Duration::from_secs(125),
        Duration::from_secs(5),
        RequestStatus::Completed,
        Some(204),
    );
    events.project = Some("provider-playground".to_string());
    events.provider = Some("codex".to_string());
    events.model = Some("gpt-5.6-luna".to_string());
    events.generation_duration = Some(Duration::from_secs(4));
    events.stream_chunks = 48;
    recent.push_back(events);

    let mut bad_model = completed_request(
        now,
        "req-failed-model",
        None,
        None,
        EndpointKind::Messages,
        Duration::from_secs(162),
        Duration::from_millis(12),
        RequestStatus::Failed,
        Some(400),
    );
    bad_model.model = Some("unknown-model".to_string());
    bad_model.error = Some("unknown model; choose a registered provider model".to_string());
    recent.push_back(bad_model);

    let mut counted = completed_request(
        now,
        "req-complete-count",
        Some("terminal-refactor"),
        Some(2),
        EndpointKind::CountTokens,
        Duration::from_secs(214),
        Duration::from_millis(84),
        RequestStatus::Completed,
        Some(200),
    );
    counted.project = Some("terminal-dashboard".to_string());
    counted.provider = Some("kimi".to_string());
    counted.model = Some("kimi-k2.6".to_string());
    counted.input_tokens = Some(2_048);
    recent.push_back(counted);

    let mut server_error = completed_request(
        now,
        "req-failed-internal",
        Some("event-stream"),
        Some(4),
        EndpointKind::Messages,
        Duration::from_secs(278),
        Duration::from_millis(420),
        RequestStatus::Failed,
        Some(500),
    );
    server_error.project = Some("provider-playground".to_string());
    server_error.provider = Some("codex".to_string());
    server_error.model = Some("gpt-5.5".to_string());
    server_error.effort = Some("medium".to_string());
    server_error.error =
        Some("response translation failed: missing message stop event".to_string());
    recent.push_back(server_error);

    let mut no_status = completed_request(
        now,
        "req-abandoned",
        Some("background-agent"),
        Some(1),
        EndpointKind::Messages,
        Duration::from_secs(340),
        Duration::from_secs(7),
        RequestStatus::Failed,
        None,
    );
    no_status.project = Some("automation-sandbox".to_string());
    no_status.provider = Some("codex".to_string());
    no_status.model = Some("gpt-5.4-mini".to_string());
    no_status.error = Some("request future ended before completion".to_string());
    recent.push_back(no_status);

    add_simulated_requests(now, instant_now, tick, &mut active, &mut recent);
    let sessions = session_summaries(&active, &recent, output_buckets);
    MonitorState {
        started_at,
        sessions,
        active,
        recent: recent.into_iter().collect(),
    }
}

fn initial_output_buckets(now: SystemTime) -> HashMap<Option<String>, Vec<(u64, u64)>> {
    const HISTORIES: [(&str, [u64; 12]); 3] = [
        (
            "57c7c914-ada4-4f40-9672-985f950fbb66",
            [0, 320, 780, 0, 1_600, 2_900, 850, 0, 440, 3_600, 2_200, 0],
        ),
        (
            "terminal-refactor",
            [180, 0, 420, 960, 0, 0, 1_300, 740, 2_100, 0, 350, 0],
        ),
        (
            "cursor-session",
            [0, 640, 1_200, 2_400, 3_800, 1_900, 0, 0, 820, 1_500, 0, 0],
        ),
    ];

    let current_bucket = super::session_token_bucket(now);
    let mut buckets = HashMap::<Option<String>, Vec<(u64, u64)>>::new();
    for (session_id, history) in HISTORIES {
        for (index, tokens) in history.iter().copied().enumerate() {
            if tokens == 0 {
                continue;
            }
            let offset = (history.len() - 1 - index) as u64;
            record_output_bucket(
                &mut buckets,
                Some(session_id.to_string()),
                current_bucket.saturating_sub(offset),
                tokens,
            );
        }
    }
    buckets
}

fn advance_output_buckets(
    buckets: &mut HashMap<Option<String>, Vec<(u64, u64)>>,
    now: SystemTime,
    tick: u64,
) {
    let current_bucket = super::session_token_bucket(now);
    record_output_bucket(
        buckets,
        Some("57c7c914-ada4-4f40-9672-985f950fbb66".to_string()),
        current_bucket,
        if tick.is_multiple_of(2) { 10 } else { 25 },
    );
    if tick.is_multiple_of(4) {
        record_output_bucket(
            buckets,
            Some("terminal-refactor".to_string()),
            current_bucket,
            120,
        );
    }
    if tick.is_multiple_of(5) {
        record_output_bucket(
            buckets,
            Some("cursor-session".to_string()),
            current_bucket,
            220,
        );
    }
    if tick % REQUEST_TICKS == COMPLETED_PHASE {
        let cycle = tick / REQUEST_TICKS;
        if cycle % 4 != 3 {
            record_output_bucket(
                buckets,
                Some(format!("demo-session-{cycle:03}")),
                current_bucket,
                128 + cycle.saturating_mul(11),
            );
        }
    }
    for samples in buckets.values_mut() {
        samples.sort_by_key(|(bucket, _)| *bucket);
    }
}

fn record_output_bucket(
    buckets: &mut HashMap<Option<String>, Vec<(u64, u64)>>,
    session_id: Option<String>,
    bucket: u64,
    tokens: u64,
) {
    let samples = buckets.entry(session_id).or_default();
    if let Some((_, existing)) = samples
        .iter_mut()
        .find(|(existing_bucket, _)| *existing_bucket == bucket)
    {
        *existing = existing.saturating_add(tokens);
    } else {
        samples.push((bucket, tokens));
    }
}

fn simulated_output_tokens(tick: u64) -> u64 {
    let pairs = tick / 2;
    pairs
        .saturating_mul(35)
        .saturating_add(if tick.is_multiple_of(2) { 0 } else { 10 })
}

fn add_simulated_requests(
    now: SystemTime,
    instant_now: Instant,
    tick: u64,
    active: &mut Vec<ActiveRequest>,
    recent: &mut VecDeque<CompletedRequest>,
) {
    let cycle = tick / REQUEST_TICKS;
    let phase = tick % REQUEST_TICKS;
    let completed_cycles = cycle + u64::from(phase >= COMPLETED_PHASE);
    let first_cycle = completed_cycles.saturating_sub(4);

    for completed_cycle in first_cycle..completed_cycles {
        let completion_tick = completed_cycle
            .saturating_mul(REQUEST_TICKS)
            .saturating_add(COMPLETED_PHASE);
        let finished_ago = Duration::from_millis(
            tick.saturating_sub(completion_tick)
                .saturating_mul(TICK_MILLIS),
        );
        recent.push_front(simulated_completed_request(
            now,
            completed_cycle,
            finished_ago,
        ));
    }

    if phase < COMPLETED_PHASE {
        active.insert(0, simulated_active_request(now, instant_now, cycle, phase));
    }
}

fn simulated_active_request(
    now: SystemTime,
    instant_now: Instant,
    cycle: u64,
    phase: u64,
) -> ActiveRequest {
    let profile = simulation_profile(cycle);
    let elapsed = Duration::from_millis(phase.saturating_mul(TICK_MILLIS));
    let status = match phase {
        0..=2 => RequestStatus::Started,
        3..=5 => RequestStatus::ProviderSelected,
        6..=9 => RequestStatus::Upstream,
        _ => RequestStatus::Streaming,
    };
    let request_id = format!("req-simulated-{cycle:03}");
    let session_id = format!("demo-session-{cycle:03}");
    let mut request = active_request(
        now,
        instant_now,
        &request_id,
        Some(&session_id),
        Some(cycle + 1),
        EndpointKind::Messages,
        elapsed,
        status,
    );
    if phase >= 2 {
        request.project = Some(profile.project.to_string());
    }
    if phase >= 3 {
        request.provider = Some(profile.provider.to_string());
        request.model = Some(profile.model.to_string());
        request.effort = profile.effort.map(str::to_string);
    }
    if phase >= 10 {
        let generation_ticks = phase - 9;
        let generation_duration =
            Duration::from_millis(generation_ticks.saturating_mul(TICK_MILLIS));
        let output_tokens = generation_ticks
            .saturating_mul(9 + cycle % 5)
            .saturating_add(generation_ticks.saturating_mul(generation_ticks) / 3);
        request.generation_started_at = Some(now - generation_duration);
        request.generation_started_instant = Some(instant_now - generation_duration);
        request.generation_finished_at = Some(now);
        request.generation_duration = Some(generation_duration);
        request.streamed_bytes = output_tokens.saturating_mul(24);
        request.stream_chunks = generation_ticks.saturating_mul(3);
        request.input_tokens = Some(1_800 + cycle.saturating_mul(137));
        request.output_tokens = Some(output_tokens);
    }
    request
}

fn simulated_completed_request(
    now: SystemTime,
    cycle: u64,
    finished_ago: Duration,
) -> CompletedRequest {
    let profile = simulation_profile(cycle);
    let failed = cycle % 4 == 3;
    let request_id = format!("req-simulated-{cycle:03}");
    let session_id = format!("demo-session-{cycle:03}");
    let latency = Duration::from_secs(5);
    let mut request = completed_request(
        now,
        &request_id,
        Some(&session_id),
        Some(cycle + 1),
        EndpointKind::Messages,
        finished_ago,
        latency,
        if failed {
            RequestStatus::Failed
        } else {
            RequestStatus::Completed
        },
        Some(if failed { 503 } else { 200 }),
    );
    request.project = Some(profile.project.to_string());
    request.provider = Some(profile.provider.to_string());
    request.model = Some(profile.model.to_string());
    request.effort = profile.effort.map(str::to_string);
    request.input_tokens = Some(1_800 + cycle.saturating_mul(137));
    if failed {
        request.error = Some("simulated upstream overload after streaming began".to_string());
    } else {
        request.generation_duration = Some(Duration::from_millis(2_750));
        request.stream_chunks = 33 + cycle % 9;
        request.output_tokens = Some(128 + cycle.saturating_mul(11));
        request.streamed_bytes = request.output_tokens.unwrap_or(0).saturating_mul(24);
    }
    request
}

#[derive(Clone, Copy)]
struct SimulationProfile {
    project: &'static str,
    provider: &'static str,
    model: &'static str,
    effort: Option<&'static str>,
}

fn simulation_profile(cycle: u64) -> SimulationProfile {
    const PROFILES: [SimulationProfile; 4] = [
        SimulationProfile {
            project: "live-dashboard",
            provider: "codex",
            model: "gpt-5.6-sol",
            effort: Some("high"),
        },
        SimulationProfile {
            project: "api-client",
            provider: "kimi",
            model: "kimi-k2.6",
            effort: Some("medium"),
        },
        SimulationProfile {
            project: "editor-extension",
            provider: "cursor",
            model: "cursor:composer-2.5-fast",
            effort: None,
        },
        SimulationProfile {
            project: "agent-workbench",
            provider: "grok",
            model: "grok-4.5",
            effort: Some("low"),
        },
    ];
    let index = usize::try_from(cycle % PROFILES.len() as u64).unwrap_or(0);
    PROFILES[index]
}

#[allow(clippy::too_many_arguments)]
fn active_request(
    now: SystemTime,
    instant_now: Instant,
    request_id: &str,
    session_id: Option<&str>,
    session_seq: Option<u64>,
    endpoint: EndpointKind,
    elapsed: Duration,
    status: RequestStatus,
) -> ActiveRequest {
    ActiveRequest {
        request_id: request_id.to_string(),
        session_id: session_id.map(str::to_string),
        session_seq,
        project: None,
        provider: None,
        model: None,
        effort: None,
        endpoint,
        started_at: now - elapsed,
        started_instant: instant_now - elapsed,
        generation_started_at: None,
        generation_started_instant: None,
        generation_initial_output_tokens: 0,
        generation_finished_at: None,
        generation_duration: None,
        status,
        streamed_bytes: 0,
        stream_chunks: 0,
        input_tokens: None,
        output_tokens: None,
        error: None,
        traffic_capture_path: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn completed_request(
    now: SystemTime,
    request_id: &str,
    session_id: Option<&str>,
    session_seq: Option<u64>,
    endpoint: EndpointKind,
    finished_ago: Duration,
    latency: Duration,
    status: RequestStatus,
    http_status: Option<u16>,
) -> CompletedRequest {
    let finished_at = now - finished_ago;
    CompletedRequest {
        request_id: request_id.to_string(),
        session_id: session_id.map(str::to_string),
        session_seq,
        project: None,
        provider: None,
        model: None,
        effort: None,
        endpoint,
        started_at: finished_at - latency,
        finished_at,
        generation_started_at: None,
        generation_started_instant: None,
        generation_initial_output_tokens: 0,
        generation_finished_at: None,
        generation_duration: None,
        status,
        http_status,
        latency,
        streamed_bytes: 0,
        stream_chunks: 0,
        input_tokens: None,
        output_tokens: None,
        error: None,
        traffic_capture_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Throughput;

    #[test]
    fn mock_state_covers_monitor_statuses_and_optional_data() {
        let state = mock_state();

        for status in [
            RequestStatus::Started,
            RequestStatus::ProviderSelected,
            RequestStatus::Upstream,
            RequestStatus::Streaming,
        ] {
            assert!(state.active.iter().any(|request| request.status == status));
        }
        assert!(
            state
                .recent
                .iter()
                .any(|request| request.status == RequestStatus::Completed)
        );
        assert!(
            state
                .recent
                .iter()
                .any(|request| request.status == RequestStatus::Failed)
        );
        assert!(state.recent.iter().any(|request| request.error.is_some()));
        assert!(
            state
                .recent
                .iter()
                .any(|request| request.traffic_capture_path.is_some())
        );
        assert!(
            state
                .recent
                .iter()
                .any(|request| request.endpoint == EndpointKind::CountTokens)
        );
        assert!(
            state
                .sessions
                .iter()
                .any(|session| session.project.is_some())
        );
        assert!(
            state
                .sessions
                .iter()
                .any(|session| session.session_id.is_none())
        );
    }

    #[test]
    fn mock_sparkline_grows_current_bucket_and_freezes_completed_buckets() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let session_id = Some("57c7c914-ada4-4f40-9672-985f950fbb66".to_string());
        let current_bucket = super::super::session_token_bucket(now);
        let mut buckets = initial_output_buckets(now);
        advance_output_buckets(&mut buckets, now, 0);
        let initial_samples = buckets.get(&session_id).unwrap().clone();
        let initial_current = initial_samples
            .iter()
            .find(|(bucket, _)| *bucket == current_bucket)
            .unwrap()
            .1;
        let initial_past = initial_samples
            .iter()
            .filter(|(bucket, _)| *bucket < current_bucket)
            .cloned()
            .collect::<Vec<_>>();

        advance_output_buckets(&mut buckets, now + Duration::from_secs(1), 1);
        let growing_samples = buckets.get(&session_id).unwrap();
        assert_eq!(
            growing_samples
                .iter()
                .find(|(bucket, _)| *bucket == current_bucket)
                .unwrap()
                .1,
            initial_current + 25
        );
        assert_eq!(
            growing_samples
                .iter()
                .filter(|(bucket, _)| *bucket < current_bucket)
                .cloned()
                .collect::<Vec<_>>(),
            initial_past
        );

        let next_bucket_time = now + Duration::from_secs(10);
        advance_output_buckets(&mut buckets, next_bucket_time, 2);
        let advanced_samples = buckets.get(&session_id).unwrap();
        assert_eq!(
            advanced_samples
                .iter()
                .find(|(bucket, _)| *bucket == current_bucket)
                .unwrap()
                .1,
            initial_current + 25
        );
        assert_eq!(
            advanced_samples
                .iter()
                .find(|(bucket, _)| *bucket == current_bucket + 1)
                .unwrap()
                .1,
            10
        );
    }

    #[test]
    fn mock_monitor_advances_tokens_rates_and_session_totals() {
        let mut monitor = MockMonitor::new();
        let first = monitor.snapshot();
        let second = monitor.snapshot();
        let first_request = first
            .active
            .iter()
            .find(|request| request.request_id == "req-active-codex")
            .unwrap();
        let second_request = second
            .active
            .iter()
            .find(|request| request.request_id == "req-active-codex")
            .unwrap();
        let first_session = first
            .sessions
            .iter()
            .find(|session| {
                session.session_id.as_deref() == Some("57c7c914-ada4-4f40-9672-985f950fbb66")
            })
            .unwrap();
        let second_session = second
            .sessions
            .iter()
            .find(|session| {
                session.session_id.as_deref() == Some("57c7c914-ada4-4f40-9672-985f950fbb66")
            })
            .unwrap();

        assert!(second_request.output_tokens > first_request.output_tokens);
        assert_ne!(second_request.rate(), first_request.rate());
        assert!(second_session.output_tokens > first_session.output_tokens);
        assert_ne!(
            second_session.output_token_samples,
            first_session.output_token_samples
        );
        assert_eq!(second.started_at, first.started_at);
    }

    #[test]
    fn mock_monitor_cycles_requests_through_lifecycle_and_new_sessions() {
        let mut monitor = MockMonitor::new();
        let states = (0..=24).map(|_| monitor.snapshot()).collect::<Vec<_>>();
        let simulated = |tick: usize| {
            states[tick]
                .active
                .iter()
                .find(|request| request.request_id.starts_with("req-simulated-"))
        };

        assert_eq!(simulated(0).unwrap().status, RequestStatus::Started);
        assert_eq!(
            simulated(3).unwrap().status,
            RequestStatus::ProviderSelected
        );
        assert_eq!(simulated(6).unwrap().status, RequestStatus::Upstream);
        assert_eq!(simulated(10).unwrap().status, RequestStatus::Streaming);
        assert!(simulated(20).is_none());
        assert!(
            states[20]
                .recent
                .iter()
                .any(|request| request.request_id == "req-simulated-000")
        );
        assert_eq!(
            simulated(24).unwrap().session_id.as_deref(),
            Some("demo-session-001")
        );
    }

    #[test]
    fn mock_state_covers_each_throughput_display() {
        let state = mock_state();
        let rates = state
            .active
            .iter()
            .map(ActiveRequest::rate)
            .chain(state.recent.iter().map(CompletedRequest::rate))
            .collect::<Vec<_>>();

        assert!(
            rates
                .iter()
                .any(|rate| matches!(rate, Throughput::TokensPerSecond(_)))
        );
        assert!(
            rates
                .iter()
                .any(|rate| matches!(rate, Throughput::BytesPerSecond(_)))
        );
        assert!(
            rates
                .iter()
                .any(|rate| matches!(rate, Throughput::EventsPerSecond(_)))
        );
        assert!(rates.iter().any(|rate| rate == &Throughput::None));
    }
}
