mod layout;

use layout::{
    CODE_WIDTH, COUNT_WIDTH, ColumnSpec, DURATION_WIDTH, EFFORT_WIDTH, ENDPOINT_WIDTH, ERROR_WIDTH,
    ID_WIDTH, LayoutTier, MODEL_MEDIUM_WIDTH, MODEL_NARROW_WIDTH, MODEL_WIDE_WIDTH,
    PROJECT_MEDIUM_WIDTH, PROJECT_WIDE_WIDTH, PROVIDER_WIDTH, RATE_WIDTH, STATUS_WIDTH, TIME_WIDTH,
    TOKEN_WIDTH,
};

use std::{
    collections::HashMap,
    io::{self, Stdout},
    sync::mpsc,
    time::{Duration, SystemTime},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use tokio::sync::oneshot;

use crate::{
    monitor::{
        ActiveRequest, CompletedRequest, MockMonitor, MonitorHandle, MonitorState,
        SESSION_TOKEN_BUCKET_SECS, SessionSummary,
    },
    paths,
    registry::Registry,
};

const TEAL: Color = Color::Rgb(78, 201, 176);
const WHITE: Color = Color::Rgb(240, 244, 248);
const DIM_WHITE: Color = Color::Rgb(180, 190, 200);
const SEPARATOR: Color = Color::Rgb(72, 74, 82);
const BG: Color = Color::Rgb(18, 18, 22);
const PANEL_BG: Color = Color::Rgb(22, 22, 27);
const SELECTED_BG: Color = Color::Rgb(42, 45, 54);
const GREEN: Color = Color::Rgb(120, 200, 120);
const RED: Color = Color::Rgb(220, 120, 120);
const YELLOW: Color = Color::Rgb(220, 200, 100);
const BLUE: Color = Color::Rgb(120, 170, 230);
const DIM: Color = Color::Rgb(100, 104, 114);
const SESSION_SPARKLINE_MIN_WIDTH: u16 = 170;
const SESSION_SPARKLINE_MAX_TOKENS: u64 = 4_000;

pub struct MonitorUiConfig<'a> {
    pub listen_url: String,
    pub port: u16,
    pub registry: &'a Registry,
    pub shutdown: Option<oneshot::Sender<()>>,
    pub shutdown_complete: Option<mpsc::Receiver<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorExit {
    ShutdownComplete,
    ForceQuit,
}

pub fn run_monitor(
    handle: MonitorHandle,
    config: MonitorUiConfig<'_>,
) -> Result<MonitorExit, anyhow::Error> {
    run_monitor_loop(|| handle.snapshot(), config, None)
}

pub fn run_mock_monitor(port: u16, registry: &Registry) -> Result<(), anyhow::Error> {
    let mut monitor = MockMonitor::new();
    run_monitor_loop(
        move || monitor.snapshot(),
        MonitorUiConfig {
            listen_url: "mock://tui-demo".to_string(),
            port,
            registry,
            shutdown: None,
            shutdown_complete: None,
        },
        Some(mock_setup_text(port, registry)),
    )
    .map(|_| ())
}

fn run_monitor_loop(
    mut snapshot: impl FnMut() -> MonitorState,
    config: MonitorUiConfig<'_>,
    setup_text_override: Option<String>,
) -> Result<MonitorExit, anyhow::Error> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = MonitorApp {
        listen_url: config.listen_url,
        setup_text: setup_text_override.unwrap_or_else(|| setup_text(config.port, config.registry)),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        tick: 0,
        phase: MonitorPhase::Running,
        shutdown: config.shutdown,
        shutdown_complete: config.shutdown_complete,
    };

    let run_result = run_monitor_events(&mut terminal, &mut snapshot, &mut app);
    if run_result.is_err() {
        app.begin_shutdown();
        let state = snapshot();
        let _ = terminal.draw(|frame| render(frame, &mut app, &state));
        app.wait_for_shutdown_completion();
    }
    let cursor_result = terminal.show_cursor();
    let exit = run_result?;
    cursor_result?;
    Ok(exit)
}

fn run_monitor_events(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut snapshot: impl FnMut() -> MonitorState,
    app: &mut MonitorApp,
) -> Result<MonitorExit, anyhow::Error> {
    loop {
        let state = snapshot();
        app.clamp_selection(state.sessions.len(), state.recent.len());
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| render(frame, app, &state))?;
        if app.shutdown_is_complete() {
            return Ok(MonitorExit::ShutdownComplete);
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.handle_ctrl_c() {
                            return Ok(MonitorExit::ForceQuit);
                        }
                    }
                    _ if app.phase == MonitorPhase::ShuttingDown => {}
                    KeyCode::Char('y') if app.phase == MonitorPhase::ConfirmingShutdown => {
                        app.begin_shutdown()
                    }
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q')
                        if app.phase == MonitorPhase::ConfirmingShutdown =>
                    {
                        app.cancel_shutdown_confirmation()
                    }
                    _ if app.phase == MonitorPhase::ConfirmingShutdown => {}
                    KeyCode::Char('q') => app.request_shutdown_confirmation(),
                    KeyCode::Char('?') => app.show_help = !app.show_help,
                    KeyCode::Char('b') => app.show_setup = !app.show_setup,
                    KeyCode::Tab => app.focus = app.focus.next(),
                    KeyCode::Down => app.move_down(state.sessions.len(), state.recent.len(), true),
                    KeyCode::Char('j') => {
                        app.move_down(state.sessions.len(), state.recent.len(), false)
                    }
                    KeyCode::Up => app.move_up(state.sessions.len(), state.recent.len(), true),
                    KeyCode::Char('k') => {
                        app.move_up(state.sessions.len(), state.recent.len(), false)
                    }
                    KeyCode::Right => app.focus = FocusPane::Recent,
                    KeyCode::Left => app.focus = FocusPane::Sessions,
                    KeyCode::Enter => {
                        app.detail = match app.focus {
                            FocusPane::Sessions if !state.sessions.is_empty() => {
                                Some(DetailView::Session)
                            }
                            FocusPane::Recent if !state.recent.is_empty() => {
                                Some(DetailView::Request)
                            }
                            _ => None,
                        }
                    }
                    KeyCode::Esc => {
                        if app.show_help {
                            app.show_help = false;
                        } else if app.show_setup {
                            app.show_setup = false;
                        } else {
                            app.detail = None;
                        }
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusPane {
    Sessions,
    Recent,
}

impl FocusPane {
    fn next(self) -> Self {
        match self {
            Self::Sessions => Self::Recent,
            Self::Recent => Self::Sessions,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailView {
    Session,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorPhase {
    Running,
    ConfirmingShutdown,
    ShuttingDown,
}

struct MonitorApp {
    listen_url: String,
    setup_text: String,
    show_setup: bool,
    show_help: bool,
    detail: Option<DetailView>,
    focus: FocusPane,
    selected: usize,
    recent_selected: usize,
    tick: usize,
    phase: MonitorPhase,
    shutdown: Option<oneshot::Sender<()>>,
    shutdown_complete: Option<mpsc::Receiver<()>>,
}

impl MonitorApp {
    fn handle_ctrl_c(&mut self) -> bool {
        if self.phase == MonitorPhase::ShuttingDown {
            true
        } else {
            self.begin_shutdown();
            false
        }
    }

    fn request_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::Running {
            self.phase = MonitorPhase::ConfirmingShutdown;
        }
    }

    fn cancel_shutdown_confirmation(&mut self) {
        if self.phase == MonitorPhase::ConfirmingShutdown {
            self.phase = MonitorPhase::Running;
        }
    }

    fn begin_shutdown(&mut self) {
        if self.phase == MonitorPhase::ShuttingDown {
            return;
        }
        self.phase = MonitorPhase::ShuttingDown;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    fn shutdown_is_complete(&self) -> bool {
        let Some(shutdown_complete) = &self.shutdown_complete else {
            return self.phase == MonitorPhase::ShuttingDown;
        };
        match shutdown_complete.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
            Err(mpsc::TryRecvError::Empty) => false,
        }
    }

    fn wait_for_shutdown_completion(&self) {
        if !self.shutdown_is_complete()
            && let Some(shutdown_complete) = &self.shutdown_complete
        {
            let _ = shutdown_complete.recv();
        }
    }

    fn clamp_selection(&mut self, sessions: usize, recent: usize) {
        self.selected = self.selected.min(sessions.saturating_sub(1));
        self.recent_selected = self.recent_selected.min(recent.saturating_sub(1));
    }

    fn move_down(&mut self, sessions: usize, recent: usize, switch_panes: bool) {
        match self.focus {
            FocusPane::Sessions => {
                if switch_panes && self.selected >= sessions.saturating_sub(1) && recent > 0 {
                    self.focus = FocusPane::Recent;
                    self.recent_selected = 0;
                } else {
                    self.selected = self
                        .selected
                        .saturating_add(1)
                        .min(sessions.saturating_sub(1));
                }
            }
            FocusPane::Recent => {
                self.recent_selected = self
                    .recent_selected
                    .saturating_add(1)
                    .min(recent.saturating_sub(1));
            }
        }
    }

    fn move_up(&mut self, sessions: usize, recent: usize, switch_panes: bool) {
        match self.focus {
            FocusPane::Sessions => self.selected = self.selected.saturating_sub(1),
            FocusPane::Recent => {
                if switch_panes && self.recent_selected == 0 && sessions > 0 {
                    self.focus = FocusPane::Sessions;
                    self.selected = sessions.saturating_sub(1);
                } else {
                    self.recent_selected = self
                        .recent_selected
                        .saturating_sub(1)
                        .min(recent.saturating_sub(1));
                }
            }
        }
    }
}

impl Drop for MonitorApp {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, anyhow::Error> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut MonitorApp, state: &MonitorState) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(30),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, root[0], app, state);
    match app.detail {
        Some(DetailView::Session) => render_session_detail(frame, root[1], state, app.selected),
        Some(DetailView::Request) => {
            render_request_detail(frame, root[1], state, app.recent_selected)
        }
        None => render_sessions(
            frame,
            root[1],
            &state.sessions,
            app.selected,
            app.focus == FocusPane::Sessions,
        ),
    }
    render_active(frame, root[2], &state.active, app.tick);
    render_recent(
        frame,
        root[3],
        &state.recent,
        app.recent_selected,
        app.focus == FocusPane::Recent,
    );
    render_events(frame, root[4], &state.recent);
    render_footer(frame, root[5], app);

    if app.show_setup {
        render_setup_overlay(frame, area, &app.setup_text);
    }
    if app.show_help {
        render_help_overlay(frame, area);
    }
    match app.phase {
        MonitorPhase::Running => {}
        MonitorPhase::ConfirmingShutdown => render_shutdown_confirmation(frame, area),
        MonitorPhase::ShuttingDown => render_shutdown_overlay(frame, area, app.tick),
    }
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    app: &MonitorApp,
    state: &MonitorState,
) {
    let uptime = state
        .started_at
        .elapsed()
        .unwrap_or_else(|_| Duration::from_secs(0));
    let text = Line::from(vec![
        Span::styled(
            " claude-code-proxy",
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(&app.listen_url, Style::default().fg(BG).bg(TEAL)),
        Span::styled("  uptime ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(
            format_duration(uptime),
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  sessions ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(
            state.sessions.len().to_string(),
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  active ", Style::default().fg(BG).bg(TEAL)),
        Span::styled(
            state.active.len().to_string(),
            Style::default()
                .fg(BG)
                .bg(TEAL)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(text).style(Style::default().bg(TEAL)), area);
}

fn panel(title: &'static str, focused: bool) -> Block<'static> {
    let color = if focused { TEAL } else { SEPARATOR };
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if focused { TEAL } else { DIM_WHITE })
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL_BG))
}

fn table_header_aligned(
    cells: impl IntoIterator<Item = (&'static str, Alignment)>,
) -> Row<'static> {
    Row::new(
        cells
            .into_iter()
            .map(|(cell, alignment)| {
                Cell::from(
                    Line::from(Span::styled(cell, Style::default().fg(TEAL))).alignment(alignment),
                )
            })
            .collect::<Vec<_>>(),
    )
    .style(Style::default().add_modifier(Modifier::BOLD))
}

fn render_empty_table_state(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &'static str,
    focused: bool,
    message: &str,
) {
    frame.render_widget(panel(title, focused), area);
    let content = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if content.width == 0 || content.height == 0 {
        return;
    }

    let line = Rect {
        y: content.y + content.height.saturating_sub(1) / 2,
        height: 1,
        ..content
    };
    frame.render_widget(
        Paragraph::new(ellipsize(message, line.width.into()))
            .alignment(Alignment::Center)
            .style(Style::default().fg(DIM).bg(PANEL_BG)),
        line,
    );
}

fn muted_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM)))
}

fn text_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
}

fn model_cell(value: Option<&str>, width: usize) -> Cell<'static> {
    text_cell(ellipsize(value.unwrap_or("-"), width))
}

fn table_column_width(area: Rect, widths: &[Constraint], column: usize) -> usize {
    let table_width = area.width.saturating_sub(2);
    Layout::horizontal(widths.to_vec())
        .spacing(1)
        .split(Rect::new(0, 0, table_width, 1))
        .get(column)
        .map_or(0, |rect| usize::from(rect.width))
}

fn ellipsize(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn display_session_id(session_id: Option<&str>) -> &str {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return "no-session";
    };
    if uuid::Uuid::parse_str(session_id).is_ok() {
        return session_id
            .split_once('-')
            .map_or(session_id, |(first, _)| first);
    }
    session_id
}

fn number_cell(value: impl Into<String>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(value.into(), Style::default().fg(DIM_WHITE)))
            .alignment(Alignment::Right),
    )
}

fn status_cell(value: &str) -> Cell<'static> {
    Cell::from(Span::styled(value.to_string(), status_style(value)))
}

fn status_style(value: &str) -> Style {
    Style::default().fg(status_color(value))
}

fn status_color(value: &str) -> Color {
    match value {
        "completed" => GREEN,
        "streaming" => TEAL,
        "failed" => RED,
        "upstream" => BLUE,
        "selected" | "started" => YELLOW,
        _ => DIM_WHITE,
    }
}

fn http_status_style(status: Option<u16>) -> Style {
    Style::default().fg(http_status_color(status))
}

fn http_status_color(status: Option<u16>) -> Color {
    match status {
        Some(200..=299) => GREEN,
        Some(400..=499) => YELLOW,
        Some(500..=599) => RED,
        Some(_) => DIM_WHITE,
        None => DIM,
    }
}

fn rate_cell(value: String) -> Cell<'static> {
    let color = if value.contains("tok/s") {
        TEAL
    } else if value == "-" {
        DIM
    } else {
        DIM_WHITE
    };
    Cell::from(
        Line::from(Span::styled(value, Style::default().fg(color))).alignment(Alignment::Right),
    )
}

fn provider_cell(value: Option<&str>) -> Cell<'static> {
    let value = value.unwrap_or("-");
    let color = match value {
        "codex" => TEAL,
        "kimi" => Color::Rgb(190, 150, 220),
        "cursor" => Color::Rgb(140, 170, 230),
        "-" => DIM,
        _ => DIM_WHITE,
    };
    Cell::from(Span::styled(value.to_string(), Style::default().fg(color)))
}

fn detail_cell(value: &str) -> Cell<'static> {
    if value.is_empty() || value == "-" {
        Cell::from(Span::styled("", Style::default().fg(DIM)))
    } else {
        Cell::from(Span::styled(value.to_string(), Style::default().fg(YELLOW)))
    }
}

fn error_indicator(request: &CompletedRequest) -> &'static str {
    if request.status == crate::monitor::RequestStatus::Failed
        || request.http_status.is_some_and(|status| status >= 400)
        || request
            .error
            .as_deref()
            .is_some_and(|error| !error.is_empty())
    {
        "!"
    } else {
        ""
    }
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn token_value(value: Option<u64>) -> String {
    value.map(compact_tokens).unwrap_or_else(|| "-".to_string())
}

fn spinner(tick: usize) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick % FRAMES.len()]
}

fn sparkline_bucket(timestamp: SystemTime) -> u64 {
    timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        / SESSION_TOKEN_BUCKET_SECS
}

fn token_sparkline(samples: &[(SystemTime, u64)], width: usize, now: SystemTime) -> String {
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if width == 0 {
        return String::new();
    }

    let mut buckets = HashMap::<u64, u64>::new();
    for (timestamp, tokens) in samples {
        let bucket = sparkline_bucket(*timestamp);
        let total = buckets.entry(bucket).or_default();
        *total = total.saturating_add(*tokens);
    }

    let current_bucket = sparkline_bucket(now);
    let first_bucket = current_bucket.saturating_sub(width.saturating_sub(1) as u64);
    (first_bucket..=current_bucket)
        .map(|bucket| {
            let value = buckets.get(&bucket).copied().unwrap_or(0);
            if value == 0 {
                return ' ';
            }
            let scaled = value.min(SESSION_SPARKLINE_MAX_TOKENS);
            let level = ((u128::from(scaled) * LEVELS.len() as u128
                + u128::from(SESSION_SPARKLINE_MAX_TOKENS)
                - 1)
                / u128::from(SESSION_SPARKLINE_MAX_TOKENS))
            .saturating_sub(1) as usize;
            LEVELS[level]
        })
        .collect()
}

fn token_sparkline_line(
    samples: &[(SystemTime, u64)],
    width: usize,
    now: SystemTime,
) -> Line<'static> {
    let mut sparkline = token_sparkline(samples, width, now);
    let current = sparkline
        .pop()
        .map_or_else(String::new, |value| value.to_string());
    Line::from(vec![
        Span::styled(sparkline, Style::default().fg(BLUE)),
        Span::styled(current, Style::default().fg(DIM)),
    ])
}

fn column_constraints<K>(columns: &[ColumnSpec<K>]) -> Vec<Constraint> {
    columns.iter().map(ColumnSpec::constraint).collect()
}

fn column_header<K>(columns: &[ColumnSpec<K>]) -> Row<'static> {
    table_header_aligned(
        columns
            .iter()
            .map(|column| (column.header, column.alignment)),
    )
}

fn target_cell(provider: Option<&str>, model: Option<&str>, width: usize) -> Cell<'static> {
    let provider = provider.unwrap_or("-");
    let model = model.unwrap_or("-");
    text_cell(ellipsize(&format!("{provider}/{model}"), width))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionColumn {
    Marker,
    Id,
    Project,
    Active,
    Requests,
    Failures,
    Counts,
    Provider,
    Model,
    Target,
    Effort,
    Input,
    Output,
    Rate,
    Activity,
    Status,
}

fn session_columns(tier: LayoutTier, show_full_sparkline: bool) -> Vec<ColumnSpec<SessionColumn>> {
    use SessionColumn as C;
    match (tier, show_full_sparkline) {
        (LayoutTier::Wide, true) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Active, "A", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Requests, "R", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Failures, "F", Alignment::Right, COUNT_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s · 4k", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Expanded | LayoutTier::Wide, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::flex(C::Activity, "Tokens/10s", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Medium, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Tok/10s", Alignment::Left, 8),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Narrow, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, 10),
            ColumnSpec::fixed(C::Counts, "A/R/F", Alignment::Right, 7),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Activity, "Trend", Alignment::Left, 6),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
        (LayoutTier::Emergency, _) => vec![
            ColumnSpec::fixed(C::Marker, "", Alignment::Left, 1),
            ColumnSpec::fixed(C::Id, "ID", Alignment::Left, ID_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
        ],
    }
}

fn render_sessions(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    sessions: &[SessionSummary],
    selected: usize,
    focused: bool,
) {
    if sessions.is_empty() {
        render_empty_table_state(frame, area, "Sessions", focused, "No sessions");
        return;
    }

    let tier = LayoutTier::for_outer_width(area.width);
    let show_full_sparkline = tier == LayoutTier::Wide && area.width >= SESSION_SPARKLINE_MIN_WIDTH;
    let columns = session_columns(tier, show_full_sparkline);
    let widths = column_constraints(&columns);
    let now = SystemTime::now();
    let rows = sessions.iter().enumerate().map(|(index, session)| {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    SessionColumn::Marker => {
                        let marker = if focused && index == selected {
                            ">"
                        } else {
                            " "
                        };
                        Cell::from(Span::styled(marker, Style::default().fg(TEAL)))
                    }
                    SessionColumn::Id => {
                        text_cell(display_session_id(session.session_id.as_deref()))
                    }
                    SessionColumn::Project => {
                        text_cell(ellipsize(session.project.as_deref().unwrap_or("-"), width))
                    }
                    SessionColumn::Active => number_cell(session.active_count.to_string()),
                    SessionColumn::Requests => number_cell(session.request_count.to_string()),
                    SessionColumn::Failures => number_cell(session.failure_count.to_string()),
                    SessionColumn::Counts => number_cell(format!(
                        "{}/{}/{}",
                        session.active_count, session.request_count, session.failure_count
                    )),
                    SessionColumn::Provider => provider_cell(session.provider.as_deref()),
                    SessionColumn::Model => model_cell(session.model.as_deref(), width),
                    SessionColumn::Target => {
                        target_cell(session.provider.as_deref(), session.model.as_deref(), width)
                    }
                    SessionColumn::Effort => text_cell(session.effort.as_deref().unwrap_or("-")),
                    SessionColumn::Input => number_cell(compact_tokens(session.input_tokens)),
                    SessionColumn::Output => number_cell(compact_tokens(session.output_tokens)),
                    SessionColumn::Rate => rate_cell(session.rate().label()),
                    SessionColumn::Activity => Cell::from(token_sparkline_line(
                        &session.output_token_samples,
                        width,
                        now,
                    )),
                    SessionColumn::Status => status_cell(&session.last_status),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Sessions", focused));
    let mut table_state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveColumn {
    Started,
    Status,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Input,
    Output,
    Rate,
    Elapsed,
}

fn active_columns(tier: LayoutTier) -> Vec<ColumnSpec<ActiveColumn>> {
    use ActiveColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::flex(C::Endpoint, "Endpoint", Alignment::Left, 1),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Started, "Started", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Status, "Status", Alignment::Left, STATUS_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Elapsed, "Elapsed", Alignment::Right, DURATION_WIDTH),
        ],
    }
}

fn render_active(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    active: &[ActiveRequest],
    tick: usize,
) {
    if active.is_empty() {
        render_empty_table_state(frame, area, "Active requests", false, "No active requests");
        return;
    }

    let columns = active_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = active.iter().map(|request| {
        let status = format!("{} {}", spinner(tick), request.status.label());
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    ActiveColumn::Started => muted_cell(format_system_time(request.started_at)),
                    ActiveColumn::Status => Cell::from(Span::styled(
                        status.clone(),
                        status_style(request.status.label()),
                    )),
                    ActiveColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    ActiveColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    ActiveColumn::Provider => provider_cell(request.provider.as_deref()),
                    ActiveColumn::Model => model_cell(request.model.as_deref(), width),
                    ActiveColumn::Target => {
                        target_cell(request.provider.as_deref(), request.model.as_deref(), width)
                    }
                    ActiveColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    ActiveColumn::Endpoint => muted_cell(request.endpoint.label()),
                    ActiveColumn::Input => number_cell(token_value(request.input_tokens)),
                    ActiveColumn::Output => number_cell(token_value(request.output_tokens)),
                    ActiveColumn::Rate => rate_cell(request.rate().label()),
                    ActiveColumn::Elapsed => number_cell(format_duration(request.elapsed())),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Active requests", false));
    frame.render_widget(table, area);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecentColumn {
    Finished,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Target,
    Effort,
    Endpoint,
    Latency,
    Rate,
    Input,
    Output,
    Details,
    Error,
}

fn recent_columns(tier: LayoutTier) -> Vec<ColumnSpec<RecentColumn>> {
    use RecentColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Endpoint, "Endpoint", Alignment::Left, ENDPOINT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::flex(C::Details, "Details", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Effort, "Effort", Alignment::Left, EFFORT_WIDTH),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::flex(C::Model, "Model", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Rate, "Rate", Alignment::Right, RATE_WIDTH),
            ColumnSpec::fixed(C::Input, "In", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Output, "Out", Alignment::Right, TOKEN_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Finished, "Finished", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Target, "Target", Alignment::Left, 1),
            ColumnSpec::fixed(C::Latency, "Latency", Alignment::Right, DURATION_WIDTH),
            ColumnSpec::fixed(C::Error, "!", Alignment::Right, ERROR_WIDTH),
        ],
    }
}

fn http_code_cell(status: Option<u16>) -> Cell<'static> {
    Cell::from(
        Line::from(Span::styled(
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "-".to_string()),
            http_status_style(status),
        ))
        .alignment(Alignment::Right),
    )
}

fn render_recent(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    recent: &[CompletedRequest],
    selected: usize,
    focused: bool,
) {
    if recent.is_empty() {
        render_empty_table_state(
            frame,
            area,
            "Recent requests",
            focused,
            "No recent requests",
        );
        return;
    }

    let columns = recent_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = recent.iter().enumerate().map(|(index, request)| {
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    RecentColumn::Finished => muted_cell(format_system_time(request.finished_at)),
                    RecentColumn::Code => http_code_cell(request.http_status),
                    RecentColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    RecentColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    RecentColumn::Provider => provider_cell(request.provider.as_deref()),
                    RecentColumn::Model => model_cell(request.model.as_deref(), width),
                    RecentColumn::Target => {
                        target_cell(request.provider.as_deref(), request.model.as_deref(), width)
                    }
                    RecentColumn::Effort => text_cell(request.effort.as_deref().unwrap_or("-")),
                    RecentColumn::Endpoint => muted_cell(request.endpoint.label()),
                    RecentColumn::Latency => number_cell(format_duration(request.latency)),
                    RecentColumn::Rate => rate_cell(request.rate().label()),
                    RecentColumn::Input => number_cell(token_value(request.input_tokens)),
                    RecentColumn::Output => number_cell(token_value(request.output_tokens)),
                    RecentColumn::Details => detail_cell(request.error.as_deref().unwrap_or("")),
                    RecentColumn::Error => detail_cell(error_indicator(request)),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(if focused && index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Recent requests", focused));
    let mut table_state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventColumn {
    Time,
    Code,
    Project,
    Session,
    Provider,
    Model,
    Message,
}

fn event_columns(tier: LayoutTier) -> Vec<ColumnSpec<EventColumn>> {
    use EventColumn as C;
    match tier {
        LayoutTier::Wide => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_WIDE_WIDTH),
            ColumnSpec::fixed(C::Session, "Session", Alignment::Left, ID_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_WIDE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Expanded => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Project, "Project", Alignment::Left, PROJECT_MEDIUM_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Medium => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_MEDIUM_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Narrow => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::fixed(C::Provider, "Provider", Alignment::Left, PROVIDER_WIDTH),
            ColumnSpec::fixed(C::Model, "Model", Alignment::Left, MODEL_NARROW_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
        LayoutTier::Emergency => vec![
            ColumnSpec::fixed(C::Time, "Time", Alignment::Left, TIME_WIDTH),
            ColumnSpec::fixed(C::Code, "Code", Alignment::Right, CODE_WIDTH),
            ColumnSpec::flex(C::Message, "Message", Alignment::Left, 1),
        ],
    }
}

fn render_events(frame: &mut ratatui::Frame<'_>, area: Rect, recent: &[CompletedRequest]) {
    let events = recent
        .iter()
        .filter(|request| {
            request.status == crate::monitor::RequestStatus::Failed
                || request.http_status.is_some_and(|status| status >= 400)
                || request.error.is_some()
        })
        .take(12)
        .collect::<Vec<_>>();
    if events.is_empty() {
        render_empty_table_state(frame, area, "Events", false, "No events");
        return;
    }

    let columns = event_columns(LayoutTier::for_outer_width(area.width));
    let widths = column_constraints(&columns);
    let rows = events.iter().map(|request| {
        let message = request
            .error
            .as_deref()
            .filter(|error| !error.is_empty())
            .unwrap_or("-");
        let cells = columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                let width = table_column_width(area, &widths, column_index);
                match column.key {
                    EventColumn::Time => muted_cell(format_system_time(request.finished_at)),
                    EventColumn::Code => http_code_cell(request.http_status),
                    EventColumn::Project => {
                        text_cell(ellipsize(request.project.as_deref().unwrap_or("-"), width))
                    }
                    EventColumn::Session => {
                        text_cell(display_session_id(request.session_id.as_deref()))
                    }
                    EventColumn::Provider => provider_cell(request.provider.as_deref()),
                    EventColumn::Model => model_cell(request.model.as_deref(), width),
                    EventColumn::Message => detail_cell(message),
                }
            })
            .collect::<Vec<_>>();
        Row::new(cells).style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths.clone())
        .header(column_header(&columns))
        .block(panel("Events", false));
    frame.render_widget(table, area);
}

fn render_session_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorState,
    selected: usize,
) {
    let lines = if let Some(session) = state.sessions.get(selected) {
        vec![
            detail_line("session", session.label(), WHITE),
            detail_line("project", session.project.as_deref().unwrap_or("-"), TEAL),
            detail_line("active requests", session.active_count.to_string(), YELLOW),
            detail_line(
                "total requests",
                session.request_count.to_string(),
                DIM_WHITE,
            ),
            detail_line("failures", session.failure_count.to_string(), RED),
            detail_line("provider", session.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line("model", session.model.as_deref().unwrap_or("-"), DIM_WHITE),
            detail_line("effort", session.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line(
                "input tokens",
                compact_tokens(session.input_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "output tokens",
                compact_tokens(session.output_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "total tokens",
                format!(
                    "{}/{}",
                    compact_tokens(session.input_tokens),
                    compact_tokens(session.output_tokens)
                ),
                DIM_WHITE,
            ),
            detail_line("rate", session.rate().label(), TEAL),
            detail_line(
                "last status",
                session.last_status.as_str(),
                status_color(&session.last_status),
            ),
        ]
    } else {
        vec![Line::from(Span::styled(
            "No session selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Session detail", true)),
        area,
    );
}

fn render_request_detail(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &MonitorState,
    selected: usize,
) {
    let lines = if let Some(request) = state.recent.get(selected) {
        let mut lines = vec![
            detail_line("request", request.request_id.clone(), WHITE),
            detail_line(
                "session",
                display_session_id(request.session_id.as_deref()),
                TEAL,
            ),
            detail_line(
                "session seq",
                request
                    .session_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                DIM_WHITE,
            ),
            detail_line("endpoint", request.endpoint.label(), DIM_WHITE),
            detail_line("started", format_system_time(request.started_at), DIM_WHITE),
            detail_line(
                "finished",
                format_system_time(request.finished_at),
                DIM_WHITE,
            ),
            detail_line(
                "status",
                request.status.label(),
                status_color(request.status.label()),
            ),
            detail_line(
                "http status",
                request
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                http_status_color(request.http_status),
            ),
            detail_line("provider", request.provider.as_deref().unwrap_or("-"), TEAL),
            detail_line("model", request.model.as_deref().unwrap_or("-"), DIM_WHITE),
            detail_line("effort", request.effort.as_deref().unwrap_or("-"), YELLOW),
            detail_line("latency", format_duration(request.latency), DIM_WHITE),
            detail_line("rate", request.rate().label(), TEAL),
            detail_line("input tokens", token_value(request.input_tokens), DIM_WHITE),
            detail_line(
                "output tokens",
                token_value(request.output_tokens),
                DIM_WHITE,
            ),
            detail_line(
                "stream bytes",
                request.streamed_bytes.to_string(),
                DIM_WHITE,
            ),
            detail_line(
                "stream chunks",
                request.stream_chunks.to_string(),
                DIM_WHITE,
            ),
        ];
        if let Some(error) = request.error.as_deref().filter(|error| !error.is_empty()) {
            lines.push(detail_line("detail", error, YELLOW));
        }
        if let Some(path) = &request.traffic_capture_path {
            lines.push(detail_line(
                "capture",
                path.to_string_lossy().into_owned(),
                DIM_WHITE,
            ));
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "No request selected",
            Style::default().fg(DIM),
        ))]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(PANEL_BG))
            .block(panel("Request detail", true)),
        area,
    );
}

fn detail_line<'a>(label: &'static str, value: impl Into<String>, value_color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<16}"), Style::default().fg(DIM)),
        Span::styled(value.into(), Style::default().fg(value_color)),
    ])
}

fn render_footer(frame: &mut ratatui::Frame<'_>, area: Rect, _app: &MonitorApp) {
    let spans = vec![
        Span::raw(" "),
        Span::styled("q", Style::default().fg(TEAL)),
        Span::styled(" quit  ", Style::default().fg(DIM)),
        Span::styled("?", Style::default().fg(TEAL)),
        Span::styled(" help  ", Style::default().fg(DIM)),
        Span::styled("b", Style::default().fg(TEAL)),
        Span::styled(" setup  ", Style::default().fg(DIM)),
        Span::styled("arrows/j/k", Style::default().fg(TEAL)),
        Span::styled(" navigate  ", Style::default().fg(DIM)),
        Span::styled("Tab", Style::default().fg(TEAL)),
        Span::styled(" pane  ", Style::default().fg(DIM)),
        Span::styled("Enter", Style::default().fg(TEAL)),
        Span::styled(" open", Style::default().fg(DIM)),
    ];
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG)),
        area,
    );

    if let Some(quota) = codex_quota_spans() {
        frame.render_widget(
            Paragraph::new(Line::from(quota))
                .alignment(Alignment::Right)
                .style(Style::default().bg(BG)),
            area,
        );
    }
}

/// Compact right-aligned Codex quota for the footer, from the latest
/// `codex.rate_limits` snapshot. Shows the weekly window as remaining
/// percent (matching the Codex app convention). None when no snapshot yet.
fn codex_quota_spans() -> Option<Vec<Span<'static>>> {
    let snapshot = crate::providers::codex::rate_limits::latest()?;
    let rl = snapshot.get("rate_limits")?;
    let primary = rl.get("primary")?;
    let used = primary.get("used_percent").and_then(|v| v.as_f64())?;
    let left = 100.0 - used;
    let color = if left > 30.0 {
        GREEN
    } else if left > 10.0 {
        YELLOW
    } else {
        RED
    };
    let mut spans = vec![
        Span::styled("codex wk ", Style::default().fg(DIM)),
        Span::styled(format!("{left:.0}% left"), Style::default().fg(color)),
    ];
    if rl.get("limit_reached").and_then(|v| v.as_bool()) == Some(true) {
        spans.push(Span::styled("  LIMIT", Style::default().fg(RED)));
    }
    spans.push(Span::raw(" "));
    Some(spans)
}

fn render_shutdown_confirmation(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let width = 44.min(area.width);
    let height = 5.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Shut down proxy?",
                Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("y", Style::default().fg(TEAL)),
                Span::styled(" confirm   ", Style::default().fg(DIM_WHITE)),
                Span::styled("n/Esc/q", Style::default().fg(TEAL)),
                Span::styled(" cancel", Style::default().fg(DIM_WHITE)),
            ]),
        ])
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(YELLOW))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().bg(BG)),
        popup,
    );
}

fn render_shutdown_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, tick: usize) {
    let width = 40.min(area.width);
    let height = 5.min(area.height);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("{} ", spinner(tick)), Style::default().fg(TEAL)),
                Span::styled(
                    "Shutting down...",
                    Style::default().fg(WHITE).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "Press Ctrl-C to force quit",
                Style::default().fg(DIM_WHITE),
            )),
        ])
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(TEAL))
                .style(Style::default().bg(BG)),
        )
        .style(Style::default().bg(BG)),
        popup,
    );
}

fn render_help_overlay(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let width = 48.min(area.width.saturating_sub(4)).max(24);
    let height = 12.min(area.height.saturating_sub(2)).max(8);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" Shortcuts ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines = [
        ("q / Ctrl-C", "quit proxy"),
        ("?", "toggle help"),
        ("b", "toggle setup"),
        ("arrows", "navigate rows and panes"),
        ("j / k", "previous / next row"),
        ("Tab", "switch pane"),
        ("Enter", "open detail"),
        ("Esc", "close overlay / detail"),
    ];
    let content = lines
        .into_iter()
        .map(|(key, label)| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{key:<10}"), Style::default().fg(TEAL)),
                Span::styled(label, Style::default().fg(DIM_WHITE)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(content).style(Style::default().bg(BG)),
        inner,
    );
}

fn render_setup_overlay(frame: &mut ratatui::Frame<'_>, area: Rect, setup_text: &str) {
    let width = 84.min(area.width.saturating_sub(4)).max(36);
    let content_height = setup_text.lines().count() as u16;
    let height = (content_height + 4)
        .min(area.height.saturating_sub(2))
        .max(8);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" Setup ", Style::default().fg(TEAL)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(TEAL))
        .style(Style::default().bg(BG));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines = setup_text
        .lines()
        .map(|line| {
            let style = if line.starts_with("export ") {
                Style::default().fg(WHITE)
            } else {
                Style::default().fg(DIM_WHITE)
            };
            Line::from(vec![Span::raw("  "), Span::styled(line.to_string(), style)])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("Esc", Style::default().fg(TEAL)),
        Span::styled(" close  ", Style::default().fg(DIM)),
        Span::styled("b", Style::default().fg(TEAL)),
        Span::styled(" toggle setup", Style::default().fg(DIM)),
    ]));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(BG))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn mock_setup_text(port: u16, registry: &Registry) -> String {
    format!(
        "Mock mode uses deterministic simulated monitor traffic.\nNo proxy server is listening.\nRun `claude-code-proxy serve` to start the proxy.\n\n{}",
        setup_text(port, registry)
    )
}

pub fn setup_text(port: u16, registry: &Registry) -> String {
    let grouped = registry.grouped_models();
    let model_summary = ["codex", "kimi", "cursor"]
        .into_iter()
        .filter_map(|provider| {
            grouped
                .get(provider)
                .map(|models| format!("{provider}: {} models", models.len()))
        })
        .collect::<Vec<_>>()
        .join("  ");
    let mut lines = vec![
        format!("Logs: {}", paths::log_file().display()),
        format!("Config: {}", paths::config_dir().display()),
        format!("Providers: {model_summary}"),
    ];
    lines.push(format!(
        "export ANTHROPIC_BASE_URL=\"http://localhost:{port}\""
    ));
    lines.push("export ANTHROPIC_AUTH_TOKEN=\"anything\"".to_string());
    lines.push("export ANTHROPIC_MODEL=\"gpt-5.6-sol\"".to_string());
    lines.push("export ANTHROPIC_SMALL_FAST_MODEL=\"gpt-5.6-luna\"".to_string());
    lines.push("export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1".to_string());
    lines.join("\n")
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_system_time(time: SystemTime) -> String {
    format_system_time_in_zone(time, TimeZone::system())
}

fn format_system_time_in_zone(time: SystemTime, time_zone: TimeZone) -> String {
    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "-".to_string();
    };
    Zoned::new(timestamp, time_zone)
        .strftime("%H:%M:%S")
        .to_string()
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, buffer::Buffer};

    use super::*;
    use crate::monitor::{EndpointKind, mock_state};

    fn draw(width: u16, height: u16, render: impl FnOnce(&mut ratatui::Frame<'_>)) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(render).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn placeholder_position(buffer: &Buffer, placeholder: &str) -> Option<(u16, u16)> {
        let symbols = placeholder
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        (0..buffer.area.height).find_map(|y| {
            (0..buffer.area.width).find_map(|x| {
                symbols
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| {
                        x + (offset as u16) < buffer.area.width
                            && buffer[(x + offset as u16, y)].symbol() == symbol
                    })
                    .then_some((x, y))
            })
        })
    }

    fn assert_centered(buffer: &Buffer, placeholder: &str, expected_y: u16) {
        let (x, y) = placeholder_position(buffer, placeholder).unwrap();
        let left_space = x.saturating_sub(1);
        let right_space = buffer
            .area
            .width
            .saturating_sub(x + placeholder.chars().count() as u16 + 1);
        assert_eq!(y, expected_y);
        assert!(left_space.abs_diff(right_space) <= 1);
    }

    fn headers<K>(columns: &[ColumnSpec<K>]) -> Vec<&'static str> {
        columns.iter().map(|column| column.header).collect()
    }

    fn fixed_budget<K>(columns: &[ColumnSpec<K>]) -> u16 {
        let widths = columns
            .iter()
            .map(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => width,
                layout::ColumnWidth::Flex(_) => 0,
            })
            .sum::<u16>();
        widths.saturating_add(columns.len().saturating_sub(1) as u16)
    }

    fn fixed_width<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<u16> {
        columns
            .iter()
            .find(|column| column.key == key)
            .and_then(|column| match column.width {
                layout::ColumnWidth::Fixed(width) => Some(width),
                layout::ColumnWidth::Flex(_) => None,
            })
    }

    fn flex_count<K>(columns: &[ColumnSpec<K>]) -> usize {
        columns
            .iter()
            .filter(|column| matches!(column.width, layout::ColumnWidth::Flex(_)))
            .count()
    }

    fn alignment<K: Copy + PartialEq>(columns: &[ColumnSpec<K>], key: K) -> Option<Alignment> {
        columns
            .iter()
            .find(|column| column.key == key)
            .map(|column| column.alignment)
    }

    #[test]
    fn format_system_time_applies_non_utc_time_zone() {
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(20 * 60 * 60);
        let time_zone = TimeZone::fixed(jiff::tz::offset(5));

        assert_eq!(format_system_time_in_zone(timestamp, time_zone), "01:00:00");
    }

    #[test]
    fn request_tables_share_time_status_provider_model_rhythm() {
        assert_eq!(
            headers(&active_columns(LayoutTier::Medium))[..4],
            ["Started", "Status", "Provider", "Model"]
        );
        assert_eq!(
            headers(&recent_columns(LayoutTier::Medium))[..4],
            ["Finished", "Code", "Provider", "Model"]
        );
        assert_eq!(
            headers(&event_columns(LayoutTier::Medium))[..4],
            ["Time", "Code", "Provider", "Model"]
        );
    }

    #[test]
    fn wide_tables_use_shared_model_and_provider_widths() {
        let sessions = session_columns(LayoutTier::Wide, true);
        let active = active_columns(LayoutTier::Wide);
        let recent = recent_columns(LayoutTier::Wide);
        let events = event_columns(LayoutTier::Wide);

        assert_eq!(
            fixed_width(&sessions, SessionColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&events, EventColumn::Model),
            Some(MODEL_WIDE_WIDTH)
        );
        assert_eq!(
            fixed_width(&active, ActiveColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
        assert_eq!(
            fixed_width(&recent, RecentColumn::Provider),
            Some(PROVIDER_WIDTH)
        );
    }

    #[test]
    fn responsive_schemas_fit_their_minimum_terminal_widths() {
        assert!(fixed_budget(&session_columns(LayoutTier::Emergency, false)) <= 75);
        assert!(fixed_budget(&session_columns(LayoutTier::Narrow, false)) <= 76);
        assert!(fixed_budget(&session_columns(LayoutTier::Medium, false)) <= 88);
        assert!(fixed_budget(&session_columns(LayoutTier::Expanded, false)) <= 118);
        assert!(fixed_budget(&session_columns(LayoutTier::Wide, true)) <= 168);

        assert!(fixed_budget(&active_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&active_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&active_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&active_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&active_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&recent_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&recent_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&recent_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&recent_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&recent_columns(LayoutTier::Wide)) <= 152);

        assert!(fixed_budget(&event_columns(LayoutTier::Emergency)) <= 75);
        assert!(fixed_budget(&event_columns(LayoutTier::Narrow)) <= 76);
        assert!(fixed_budget(&event_columns(LayoutTier::Medium)) <= 88);
        assert!(fixed_budget(&event_columns(LayoutTier::Expanded)) <= 118);
        assert!(fixed_budget(&event_columns(LayoutTier::Wide)) <= 152);
    }

    #[test]
    fn active_table_renders_expected_headers_at_tier_boundaries() {
        let state = mock_state();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_active(frame, frame.area(), &state.active, 0)
            });
            buffer_text(&buffer)
        };

        let emergency = render_at(77);
        assert!(emergency.contains("Started"), "{emergency}");
        assert!(emergency.contains("Target"), "{emergency}");
        assert!(!emergency.contains("Rate"), "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Provider"), "{narrow}");
        assert!(narrow.contains("Model"), "{narrow}");
        assert!(narrow.contains("Effort"), "{narrow}");
        assert!(!narrow.contains("Project"), "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Provider"), "{medium}");
        assert!(medium.contains("Model"), "{medium}");
        assert!(medium.contains("Endpoint"), "{medium}");
        assert!(!medium.contains("Project"), "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Project"), "{expanded}");
        assert!(expanded.contains("Session"), "{expanded}");
        assert!(expanded.contains("Endpoint"), "{expanded}");
        assert!(!expanded.contains("In"), "{expanded}");

        let wide = render_at(154);
        assert!(wide.contains("Project"), "{wide}");
        assert!(wide.contains("Session"), "{wide}");
        assert!(wide.contains("In"), "{wide}");
        assert!(wide.contains("Out"), "{wide}");
    }

    #[test]
    fn each_schema_has_one_meaningful_flexible_column() {
        for tier in [
            LayoutTier::Emergency,
            LayoutTier::Narrow,
            LayoutTier::Medium,
            LayoutTier::Expanded,
            LayoutTier::Wide,
        ] {
            let sessions = session_columns(tier, tier == LayoutTier::Wide);
            let active = active_columns(tier);
            let recent = recent_columns(tier);
            let events = event_columns(tier);

            assert_eq!(flex_count(&sessions), 1);
            assert_eq!(flex_count(&active), 1);
            assert_eq!(flex_count(&recent), 1);
            assert_eq!(flex_count(&events), 1);
            assert_eq!(
                sessions
                    .iter()
                    .filter(|column| column.header.is_empty())
                    .count(),
                1
            );
            assert!(active.iter().all(|column| !column.header.is_empty()));
            assert!(recent.iter().all(|column| !column.header.is_empty()));
            assert!(events.iter().all(|column| !column.header.is_empty()));
        }
    }

    #[test]
    fn metric_columns_are_right_aligned() {
        let sessions = session_columns(LayoutTier::Wide, true);
        for key in [
            SessionColumn::Active,
            SessionColumn::Requests,
            SessionColumn::Failures,
            SessionColumn::Input,
            SessionColumn::Output,
            SessionColumn::Rate,
        ] {
            assert_eq!(alignment(&sessions, key), Some(Alignment::Right));
        }

        let active = active_columns(LayoutTier::Wide);
        for key in [
            ActiveColumn::Input,
            ActiveColumn::Output,
            ActiveColumn::Rate,
            ActiveColumn::Elapsed,
        ] {
            assert_eq!(alignment(&active, key), Some(Alignment::Right));
        }

        let recent = recent_columns(LayoutTier::Wide);
        for key in [
            RecentColumn::Code,
            RecentColumn::Latency,
            RecentColumn::Rate,
            RecentColumn::Input,
            RecentColumn::Output,
        ] {
            assert_eq!(alignment(&recent, key), Some(Alignment::Right));
        }
    }

    #[test]
    fn narrow_schemas_use_available_space_for_context() {
        let sessions = session_columns(LayoutTier::Narrow, false);
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Project)
        );
        assert!(
            sessions
                .iter()
                .any(|column| column.key == SessionColumn::Target)
        );

        let active = active_columns(LayoutTier::Narrow);
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Provider)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Model)
        );
        assert!(
            active
                .iter()
                .any(|column| column.key == ActiveColumn::Effort)
        );

        let recent = recent_columns(LayoutTier::Narrow);
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Provider)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Input)
        );
        assert!(
            recent
                .iter()
                .any(|column| column.key == RecentColumn::Output)
        );

        let events = event_columns(LayoutTier::Emergency);
        assert_eq!(headers(&events), ["Time", "Code", "Message"]);
    }

    #[test]
    fn display_session_id_shortens_uuids() {
        assert_eq!(
            display_session_id(Some("57c7c914-ada4-4f40-9672-985f950fbb66")),
            "57c7c914"
        );
    }

    #[test]
    fn display_session_id_handles_atypical_ids() {
        assert_eq!(display_session_id(Some("custom-session")), "custom-session");
        assert_eq!(display_session_id(Some("")), "no-session");
        assert_eq!(display_session_id(None), "no-session");
    }

    #[test]
    fn ellipsize_marks_truncated_values() {
        assert_eq!(ellipsize("claude-sonnet-4-6", 16), "claude-sonnet-4…");
        assert_eq!(ellipsize("gpt-5.6-sol", 16), "gpt-5.6-sol");
        assert_eq!(ellipsize("anything", 0), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_wall_clock_buckets() {
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(78), 2_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(85), 3_000),
            (SystemTime::UNIX_EPOCH + Duration::from_secs(100), 4_000),
        ];
        let bucket_start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let bucket_end = SystemTime::UNIX_EPOCH + Duration::from_secs(109);
        let next_bucket = SystemTime::UNIX_EPOCH + Duration::from_secs(110);

        assert_eq!(token_sparkline(&[], 4, bucket_start), "    ");
        assert_eq!(token_sparkline(&samples, 4, bucket_start), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, bucket_end), "▄▆ █");
        assert_eq!(token_sparkline(&samples, 4, next_bucket), "▆ █ ");
        assert_eq!(token_sparkline(&samples, 0, bucket_start), "");
    }

    #[test]
    fn token_sparkline_uses_fixed_shared_scale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let current = (now, 2_000);
        let offscreen_peak = (SystemTime::UNIX_EPOCH, 4_000);

        assert_eq!(token_sparkline(&[current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[offscreen_peak, current], 2, now), " ▄");
        assert_eq!(token_sparkline(&[(now, 10_000)], 1, now), "█");
    }

    #[test]
    fn token_sparkline_dims_the_current_bucket() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let samples = [
            (SystemTime::UNIX_EPOCH + Duration::from_secs(95), 2_000),
            (now, 4_000),
        ];

        let line = token_sparkline_line(&samples, 2, now);

        assert_eq!(line.spans[0].content, "▄");
        assert_eq!(line.spans[0].style.fg, Some(BLUE));
        assert_eq!(line.spans[1].content, "█");
        assert_eq!(line.spans[1].style.fg, Some(DIM));
    }

    #[test]
    fn session_sparkline_appears_at_medium_width_and_expands() {
        let monitor = MonitorHandle::new(10);
        for (index, tokens) in [1_000, 2_000, 4_000].into_iter().enumerate() {
            let request_id = format!("request-{index}");
            monitor.request_started(
                &request_id,
                Some("sess-1".to_string()),
                Some(index as u64 + 1),
                EndpointKind::Messages,
            );
            monitor.provider_selected(&request_id, "codex", "gpt-5.6-sol", None);
            monitor.request_completed(&request_id, 200, Some(100), Some(tokens));
        }
        let state = monitor.snapshot();
        let render_at = |width| {
            let buffer = draw(width, 8, |frame| {
                render_sessions(frame, frame.area(), &state.sessions, 0, true)
            });
            buffer_text(&buffer)
        };
        let spark_chars = |text: &str| {
            text.chars()
                .filter(|ch| matches!(ch, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'))
                .count()
        };

        let emergency = render_at(77);
        assert!(!emergency.contains("Trend"), "{emergency}");
        assert_eq!(spark_chars(&emergency), 0, "{emergency}");

        let narrow = render_at(78);
        assert!(narrow.contains("Trend"), "{narrow}");
        assert!(spark_chars(&narrow) > 0, "{narrow}");

        let medium = render_at(90);
        assert!(medium.contains("Tok/10s"), "{medium}");
        assert!(spark_chars(&medium) > 0, "{medium}");

        let expanded = render_at(120);
        assert!(expanded.contains("Tokens/10s"), "{expanded}");
        assert!(spark_chars(&expanded) > 0, "{expanded}");

        let wide = render_at(SESSION_SPARKLINE_MIN_WIDTH);
        assert!(wide.contains("Tokens/10s · 4k"), "{wide}");
        assert!(spark_chars(&wide) > 0, "{wide}");
    }

    #[test]
    fn empty_tables_hide_columns_and_center_placeholders() {
        let sessions = draw(40, 9, |frame| {
            render_sessions(frame, frame.area(), &[], 0, true)
        });
        let sessions_text = buffer_text(&sessions);
        assert_centered(&sessions, "No sessions", 4);
        assert!(!sessions_text.contains("provider"));
        assert!(sessions_text.contains("No sessions"));

        let active = draw(27, 6, |frame| render_active(frame, frame.area(), &[], 0));
        let active_text = buffer_text(&active);
        assert_centered(&active, "No active requests", 2);
        assert!(!active_text.contains("started"));
        assert!(active_text.contains("No active requests"));

        let recent = draw(40, 9, |frame| {
            render_recent(frame, frame.area(), &[], 0, false)
        });
        let recent_text = buffer_text(&recent);
        assert_centered(&recent, "No recent requests", 4);
        assert!(!recent_text.contains("finished"));
        assert!(recent_text.contains("No recent requests"));

        let events = draw(40, 9, |frame| render_events(frame, frame.area(), &[]));
        let events_text = buffer_text(&events);
        assert_centered(&events, "No events", 4);
        assert!(!events_text.contains("time"));
        assert!(events_text.contains("No events"));
    }

    #[test]
    fn active_status_keeps_full_label_at_narrow_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.upstream_started("request-1");
        let state = monitor.snapshot();

        let active = draw(88, 6, |frame| {
            render_active(frame, frame.area(), &state.active, 0)
        });

        let active_text = buffer_text(&active);
        assert!(active_text.contains("⠋ upstream"), "{active_text}");
    }

    #[test]
    fn populated_tables_render_rows_without_placeholders() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(1),
            EndpointKind::Messages,
        );
        monitor.project_resolved("request-1", "example-project");
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        let active_state = monitor.snapshot();

        let sessions = draw(170, 8, |frame| {
            render_sessions(frame, frame.area(), &active_state.sessions, 0, true)
        });
        let sessions_text = buffer_text(&sessions);
        assert!(sessions_text.contains("Provider"));
        assert!(sessions_text.contains("Project"));
        assert!(sessions_text.contains("example-project"));
        assert!(sessions_text.contains("sess-1"));
        assert!(!sessions_text.contains("No sessions"));

        let active = draw(120, 8, |frame| {
            render_active(frame, frame.area(), &active_state.active, 0)
        });
        let active_text = buffer_text(&active);
        assert!(active_text.contains("Started"));
        assert!(active_text.contains("gpt-5.6-sol"));
        assert!(!active_text.contains("No active requests"));

        monitor.request_completed("request-1", 200, Some(100), Some(25));
        let completed_state = monitor.snapshot();
        let recent = draw(140, 8, |frame| {
            render_recent(frame, frame.area(), &completed_state.recent, 0, false)
        });
        let recent_text = buffer_text(&recent);
        assert!(recent_text.contains("Finished"));
        assert!(recent_text.contains("200"));
        assert!(!recent_text.contains("No recent requests"));

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &completed_state.recent)
        });
        assert!(buffer_text(&events).contains("No events"));
    }

    #[test]
    fn selected_rows_scroll_into_table_viewports() {
        let state = mock_state();
        let sessions = (0..12)
            .map(|index| {
                let mut session = state.sessions[0].clone();
                session.session_id = Some(format!("row-{index:04}"));
                session
            })
            .collect::<Vec<_>>();
        let session_buffer = draw(120, 6, |frame| {
            render_sessions(frame, frame.area(), &sessions, 11, true)
        });
        let session_text = buffer_text(&session_buffer);
        assert!(session_text.contains("row-0011"), "{session_text}");
        assert!(!session_text.contains("row-0000"), "{session_text}");

        let recent = (0..12)
            .map(|index| {
                let mut request = state.recent[0].clone();
                request.project = Some(format!("row-{index:04}"));
                request
            })
            .collect::<Vec<_>>();
        let recent_buffer = draw(120, 6, |frame| {
            render_recent(frame, frame.area(), &recent, 11, true)
        });
        let recent_text = buffer_text(&recent_buffer);
        assert!(recent_text.contains("row-0011"), "{recent_text}");
        assert!(!recent_text.contains("row-0000"), "{recent_text}");
    }

    #[test]
    fn mock_state_renders_representative_panes_at_wide_width() {
        let state = mock_state();
        let mut app = MonitorApp {
            listen_url: "mock://tui-demo".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: None,
        };

        let buffer = draw(180, 48, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&buffer);

        assert!(text.contains("mock://tui-demo"), "{text}");
        assert!(text.contains("claude-code-proxy"), "{text}");
        assert!(text.contains("streaming"), "{text}");
        assert!(text.contains("gpt-5.6-terra"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
    }

    #[test]
    fn mock_request_detail_exposes_error_and_capture_fields() {
        let state = mock_state();
        let failed = state
            .recent
            .iter()
            .position(|request| request.request_id == "req-failed-kimi")
            .unwrap();

        let detail = draw(140, 22, |frame| {
            render_request_detail(frame, frame.area(), &state, failed)
        });
        let text = buffer_text(&detail);

        assert!(text.contains("req-failed-kimi"), "{text}");
        assert!(text.contains("upstream connection closed"), "{text}");
        assert!(text.contains("req-failed-kimi.json"), "{text}");
    }

    #[test]
    fn recent_table_uses_error_indicator_at_medium_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let recent = draw(110, 8, |frame| {
            render_recent(frame, frame.area(), &state.recent, 0, true)
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("!"), "{recent_text}");
        assert!(!recent_text.contains("Details"), "{recent_text}");
        assert!(
            !recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn recent_table_keeps_detail_text_at_wide_width() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.provider_selected("request-1", "codex", "gpt-5.6-sol", None);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let recent = draw(180, 8, |frame| {
            render_recent(frame, frame.area(), &state.recent, 0, false)
        });
        let recent_text = buffer_text(&recent);

        assert!(recent_text.contains("Details"), "{recent_text}");
        assert!(
            recent_text.contains("upstream unavailable"),
            "{recent_text}"
        );
    }

    #[test]
    fn request_detail_renders_full_error_text() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started(
            "request-1",
            Some("sess-1".to_string()),
            Some(7),
            EndpointKind::Messages,
        );
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let detail = draw(120, 20, |frame| {
            render_request_detail(frame, frame.area(), &state, 0)
        });
        let detail_text = buffer_text(&detail);

        assert!(detail_text.contains("Request detail"), "{detail_text}");
        assert!(detail_text.contains("request-1"), "{detail_text}");
        assert!(detail_text.contains("sess-1"), "{detail_text}");
        assert!(
            detail_text.contains("upstream unavailable"),
            "{detail_text}"
        );
    }

    #[test]
    fn events_render_matching_request_rows_without_a_placeholder() {
        let monitor = MonitorHandle::new(10);
        monitor.request_started("request-1", None, None, EndpointKind::Messages);
        monitor.request_failed("request-1", Some(502), "upstream unavailable");
        let state = monitor.snapshot();

        let events = draw(100, 8, |frame| {
            render_events(frame, frame.area(), &state.recent)
        });
        let events_text = buffer_text(&events);
        assert!(events_text.contains("Time"));
        assert!(events_text.contains("502"));
        assert!(events_text.contains("upstream unavailable"));
        assert!(!events_text.contains("No events"));
    }

    #[test]
    fn shutdown_confirmation_can_be_cancelled_before_signalling_server() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.request_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::ConfirmingShutdown);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        let state = MonitorHandle::default().snapshot();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shut down proxy?"), "{text}");
        assert!(text.contains("y confirm"), "{text}");
        assert!(text.contains("n/Esc/q cancel"), "{text}");

        app.cancel_shutdown_confirmation();
        assert_eq!(app.phase, MonitorPhase::Running);
        assert!(matches!(
            shutdown_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        app.request_shutdown_confirmation();
        app.begin_shutdown();
        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
    }

    #[test]
    fn ctrl_c_starts_shutdown_then_requests_force_quit() {
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let (_shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel();
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: Some(shutdown_tx),
            shutdown_complete: Some(shutdown_complete_rx),
        };

        assert!(!app.handle_ctrl_c());
        assert!(app.handle_ctrl_c());

        assert_eq!(app.phase, MonitorPhase::ShuttingDown);
        assert_eq!(shutdown_rx.try_recv(), Ok(()));
        let state = MonitorHandle::default().snapshot();
        let screen = draw(80, 24, |frame| render(frame, &mut app, &state));
        let text = buffer_text(&screen);
        assert!(text.contains("Shutting down..."));
        assert!(text.contains("Press Ctrl-C to force quit"));
    }

    #[test]
    fn shutdown_completion_accepts_notification_and_sender_drop() {
        let (complete_tx, complete_rx) = mpsc::channel();
        let app = MonitorApp {
            listen_url: String::new(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(complete_rx),
        };

        assert!(!app.shutdown_is_complete());
        complete_tx.send(()).unwrap();
        assert!(app.shutdown_is_complete());

        let (complete_tx, complete_rx) = mpsc::channel();
        let mut app = app;
        app.shutdown_complete = Some(complete_rx);
        drop(complete_tx);
        assert!(app.shutdown_is_complete());
    }

    #[test]
    fn header_renders_configured_listen_url() {
        let app = MonitorApp {
            listen_url: "http://[::]:18765".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 0,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };
        let state = MonitorHandle::default().snapshot();

        let header = draw(100, 1, |frame| {
            render_header(frame, frame.area(), &app, &state)
        });

        assert!(buffer_text(&header).contains("http://[::]:18765"));
    }

    #[test]
    fn clamp_selection_caps_to_available_sessions() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 10,
            recent_selected: 10,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.clamp_selection(3, 4);
        assert_eq!(app.selected, 2);
        assert_eq!(app.recent_selected, 3);

        app.clamp_selection(0, 0);
        assert_eq!(app.selected, 0);
        assert_eq!(app.recent_selected, 0);
    }

    #[test]
    fn arrow_navigation_moves_between_focus_panes_at_edges() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 1,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.move_down(2, 3, true);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected, 0);

        app.move_up(2, 3, true);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn vim_navigation_stays_within_focused_pane() {
        let mut app = MonitorApp {
            listen_url: "http://127.0.0.1:3000".to_string(),
            setup_text: String::new(),
            show_setup: false,
            show_help: false,
            detail: None,
            focus: FocusPane::Sessions,
            selected: 1,
            recent_selected: 0,
            tick: 0,
            phase: MonitorPhase::Running,
            shutdown: None,
            shutdown_complete: Some(mpsc::channel().1),
        };

        app.move_down(2, 3, false);
        assert_eq!(app.focus, FocusPane::Sessions);
        assert_eq!(app.selected, 1);

        app.focus = FocusPane::Recent;
        app.move_up(2, 3, false);
        assert_eq!(app.focus, FocusPane::Recent);
        assert_eq!(app.recent_selected, 0);
    }
}
