use std::{
    io::{self, Stdout},
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
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};
use tokio::sync::oneshot;

use crate::{
    monitor::{ActiveRequest, CompletedRequest, MonitorHandle, MonitorState, SessionSummary},
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
const RECENT_MODEL_WIDTH: u16 = 36;

const SESSION_TABLE_HEADERS: [(&str, Alignment); 12] = [
    ("", Alignment::Left),
    ("ID", Alignment::Left),
    ("Active", Alignment::Right),
    ("Reqs", Alignment::Right),
    ("Fail", Alignment::Right),
    ("Provider", Alignment::Left),
    ("Model", Alignment::Left),
    ("Effort", Alignment::Left),
    ("In", Alignment::Right),
    ("Out", Alignment::Right),
    ("Rate", Alignment::Right),
    ("Status", Alignment::Left),
];

const ACTIVE_TABLE_HEADERS: [(&str, Alignment); 8] = [
    ("Started", Alignment::Left),
    ("Provider", Alignment::Left),
    ("Model", Alignment::Left),
    ("Effort", Alignment::Left),
    ("Endpoint", Alignment::Left),
    ("Status", Alignment::Left),
    ("Rate", Alignment::Right),
    ("Elapsed", Alignment::Right),
];

const RECENT_TABLE_HEADERS: [(&str, Alignment); 10] = [
    ("Finished", Alignment::Left),
    ("Status", Alignment::Left),
    ("Provider", Alignment::Left),
    ("Model", Alignment::Left),
    ("Effort", Alignment::Left),
    ("Latency", Alignment::Right),
    ("Rate", Alignment::Right),
    ("In", Alignment::Right),
    ("Out", Alignment::Right),
    ("Details", Alignment::Left),
];

const RECENT_INDICATOR_TABLE_HEADERS: [(&str, Alignment); 10] = [
    ("Finished", Alignment::Left),
    ("Status", Alignment::Left),
    ("Provider", Alignment::Left),
    ("Model", Alignment::Left),
    ("Effort", Alignment::Left),
    ("Latency", Alignment::Right),
    ("Rate", Alignment::Right),
    ("In", Alignment::Right),
    ("Out", Alignment::Right),
    ("Err", Alignment::Left),
];

const RECENT_DETAIL_WIDTH: u16 = 132;

const EVENTS_TABLE_HEADERS: [(&str, Alignment); 5] = [
    ("Time", Alignment::Left),
    ("Status", Alignment::Left),
    ("Provider", Alignment::Left),
    ("Model", Alignment::Left),
    ("Message", Alignment::Left),
];

pub struct MonitorUiConfig<'a> {
    pub listen_url: String,
    pub port: u16,
    pub registry: &'a Registry,
    pub shutdown: Option<oneshot::Sender<()>>,
}

pub fn run_monitor(
    handle: MonitorHandle,
    config: MonitorUiConfig<'_>,
) -> Result<(), anyhow::Error> {
    let mut terminal = setup_terminal()?;
    let _guard = TerminalGuard;
    let mut app = MonitorApp {
        listen_url: config.listen_url,
        setup_text: setup_text(config.port, config.registry),
        show_setup: false,
        show_help: false,
        detail: None,
        focus: FocusPane::Sessions,
        selected: 0,
        recent_selected: 0,
        tick: 0,
        shutdown: config.shutdown,
    };

    loop {
        let state = handle.snapshot();
        app.clamp_selection(state.sessions.len(), state.recent.len());
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| render(frame, &mut app, &state))?;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => match key.code {
                    KeyCode::Char('q') => {
                        if let Some(shutdown) = app.shutdown.take() {
                            let _ = shutdown.send(());
                        }
                        break;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(shutdown) = app.shutdown.take() {
                            let _ = shutdown.send(());
                        }
                        break;
                    }
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
    terminal.show_cursor()?;
    Ok(())
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
    shutdown: Option<oneshot::Sender<()>>,
}

impl MonitorApp {
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
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
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

    let widths = [
        Constraint::Length(1),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Percentage(20),
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(12),
        Constraint::Length(10),
    ];
    let model_width = table_column_width(area, &widths, 6);
    let rows = sessions.iter().enumerate().map(|(index, session)| {
        let marker = if focused && index == selected {
            ">"
        } else {
            " "
        };
        Row::new(vec![
            Cell::from(Span::styled(marker, Style::default().fg(TEAL))),
            text_cell(display_session_id(session.session_id.as_deref())),
            number_cell(session.active_count.to_string()),
            number_cell(session.request_count.to_string()),
            number_cell(session.failure_count.to_string()),
            provider_cell(session.provider.as_deref()),
            model_cell(session.model.as_deref(), model_width),
            text_cell(session.effort.as_deref().unwrap_or("-")),
            number_cell(compact_tokens(session.input_tokens)),
            number_cell(compact_tokens(session.output_tokens)),
            rate_cell(session.rate().label()),
            status_cell(&session.last_status),
        ])
        .style(if index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let table = Table::new(rows, widths)
        .header(table_header_aligned(SESSION_TABLE_HEADERS))
        .block(panel("Sessions", focused));
    frame.render_widget(table, area);
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

    let widths = [
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Min(18),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(9),
    ];
    let model_width = table_column_width(area, &widths, 2);
    let rows = active.iter().map(|request| {
        let status = if matches!(
            request.status.label(),
            "upstream" | "streaming" | "selected" | "started"
        ) {
            format!("{} {}", spinner(tick), request.status.label())
        } else {
            request.status.label().to_string()
        };
        Row::new(vec![
            muted_cell(format_system_time(request.started_at)),
            provider_cell(request.provider.as_deref()),
            model_cell(request.model.as_deref(), model_width),
            text_cell(request.effort.as_deref().unwrap_or("-")),
            muted_cell(request.endpoint.label()),
            Cell::from(Span::styled(status, status_style(request.status.label()))),
            rate_cell(request.rate().label()),
            number_cell(format_duration(request.elapsed())),
        ])
        .style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths)
        .header(table_header_aligned(ACTIVE_TABLE_HEADERS))
        .block(panel("Active requests", false));
    frame.render_widget(table, area);
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

    let show_detail = area.width >= RECENT_DETAIL_WIDTH;
    let widths = if show_detail {
        vec![
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(RECENT_MODEL_WIDTH),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Fill(1),
        ]
    } else {
        vec![
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(if area.width >= 105 { 18 } else { 12 }),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(3),
        ]
    };
    let model_width = table_column_width(area, &widths, 3);
    let rows = recent.iter().enumerate().map(|(index, request)| {
        let detail = if show_detail {
            request.error.as_deref().unwrap_or("")
        } else {
            error_indicator(request)
        };
        Row::new(vec![
            muted_cell(format_system_time(request.finished_at)),
            Cell::from(Span::styled(
                request
                    .http_status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                http_status_style(request.http_status),
            )),
            provider_cell(request.provider.as_deref()),
            model_cell(request.model.as_deref(), model_width),
            text_cell(request.effort.as_deref().unwrap_or("-")),
            number_cell(format_duration(request.latency)),
            rate_cell(request.rate().label()),
            number_cell(token_value(request.input_tokens)),
            number_cell(token_value(request.output_tokens)),
            detail_cell(detail),
        ])
        .style(if focused && index == selected {
            Style::default().bg(SELECTED_BG)
        } else {
            Style::default().bg(PANEL_BG)
        })
    });
    let headers = if show_detail {
        RECENT_TABLE_HEADERS
    } else {
        RECENT_INDICATOR_TABLE_HEADERS
    };
    let table = Table::new(rows, widths)
        .header(table_header_aligned(headers))
        .block(panel("Recent requests", focused));
    frame.render_widget(table, area);
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

    let widths = [
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Min(18),
        Constraint::Percentage(50),
    ];
    let model_width = table_column_width(area, &widths, 3);
    let rows = events.iter().map(|request| {
        let status = request
            .http_status
            .map(|status| status.to_string())
            .unwrap_or_else(|| request.status.label().to_string());
        let message = request
            .error
            .as_deref()
            .filter(|error| !error.is_empty())
            .unwrap_or("-");
        Row::new(vec![
            muted_cell(format_system_time(request.finished_at)),
            Cell::from(Span::styled(status, http_status_style(request.http_status))),
            provider_cell(request.provider.as_deref()),
            model_cell(request.model.as_deref(), model_width),
            detail_cell(message),
        ])
        .style(Style::default().bg(PANEL_BG))
    });
    let table = Table::new(rows, widths)
        .header(table_header_aligned(EVENTS_TABLE_HEADERS))
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
    use crate::monitor::EndpointKind;

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

    fn table_header_labels<'a, const N: usize>(
        headers: &'a [(&'a str, Alignment); N],
    ) -> [&'a str; N] {
        headers.map(|(label, _)| label)
    }

    #[test]
    fn format_system_time_applies_non_utc_time_zone() {
        let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(20 * 60 * 60);
        let time_zone = TimeZone::fixed(jiff::tz::offset(5));

        assert_eq!(format_system_time_in_zone(timestamp, time_zone), "01:00:00");
    }

    #[test]
    fn table_headers_use_expected_labels() {
        assert_eq!(
            table_header_labels(&SESSION_TABLE_HEADERS),
            [
                "", "ID", "Active", "Reqs", "Fail", "Provider", "Model", "Effort", "In", "Out",
                "Rate", "Status",
            ]
        );
        assert_eq!(
            table_header_labels(&ACTIVE_TABLE_HEADERS),
            [
                "Started", "Provider", "Model", "Effort", "Endpoint", "Status", "Rate", "Elapsed",
            ]
        );
        assert_eq!(
            table_header_labels(&RECENT_TABLE_HEADERS),
            [
                "Finished", "Status", "Provider", "Model", "Effort", "Latency", "Rate", "In",
                "Out", "Details",
            ]
        );
        assert_eq!(
            table_header_labels(&RECENT_INDICATOR_TABLE_HEADERS),
            [
                "Finished", "Status", "Provider", "Model", "Effort", "Latency", "Rate", "In",
                "Out", "Err",
            ]
        );
        assert_eq!(
            table_header_labels(&EVENTS_TABLE_HEADERS),
            ["Time", "Status", "Provider", "Model", "Message"]
        );
        assert_eq!(ACTIVE_TABLE_HEADERS[6], ("Rate", Alignment::Right));
        assert_eq!(RECENT_TABLE_HEADERS[6], ("Rate", Alignment::Right));
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
    fn recent_rate_column_keeps_full_width() {
        let widths = [
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(RECENT_MODEL_WIDTH),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Fill(1),
        ];

        assert_eq!(table_column_width(Rect::new(0, 0, 90, 10), &widths, 6), 12);
        assert_eq!(table_column_width(Rect::new(0, 0, 90, 10), &widths, 7), 7);
        assert_eq!(table_column_width(Rect::new(0, 0, 90, 10), &widths, 8), 7);
        assert_eq!(
            table_column_width(Rect::new(0, 0, 200, 10), &widths, 3),
            usize::from(RECENT_MODEL_WIDTH)
        );
        assert!(
            usize::from(RECENT_MODEL_WIDTH) >= "claude-sonnet-4-6 → gpt-5.6-terra".chars().count()
        );
    }

    #[test]
    fn model_column_width_tracks_terminal_width() {
        let widths = [
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(18),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(9),
        ];
        let narrow = table_column_width(Rect::new(0, 0, 110, 10), &widths, 2);
        let wide = table_column_width(Rect::new(0, 0, 212, 10), &widths, 2);

        assert!(wide > narrow);
        assert!(wide >= "claude-haiku-4-5 → gpt-5.6-luna".chars().count());
    }

    #[test]
    fn ellipsize_marks_truncated_values() {
        assert_eq!(ellipsize("claude-sonnet-4-6", 16), "claude-sonnet-4…");
        assert_eq!(ellipsize("gpt-5.6-sol", 16), "gpt-5.6-sol");
        assert_eq!(ellipsize("anything", 0), "");
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
        monitor.provider_selected(
            "request-1",
            "codex",
            "gpt-5.6-sol",
            Some("high".to_string()),
        );
        let active_state = monitor.snapshot();

        let sessions = draw(160, 8, |frame| {
            render_sessions(frame, frame.area(), &active_state.sessions, 0, true)
        });
        let sessions_text = buffer_text(&sessions);
        assert!(sessions_text.contains("Provider"));
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

        assert!(recent_text.contains("Err"), "{recent_text}");
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

        let recent = draw(150, 8, |frame| {
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
            shutdown: None,
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
            shutdown: None,
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
            shutdown: None,
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
            shutdown: None,
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
