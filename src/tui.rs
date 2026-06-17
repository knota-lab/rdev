use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::AppConfig;
use crate::error::{err_with_source, Result};
use crate::error_info;

const INPUT_PROMPT: &str = "rdev> ";
const PROCESS_PANEL_MIN_WIDTH: u16 = 24;
const PROCESS_PANEL_MAX_WIDTH: u16 = 36;

#[derive(Debug, Clone)]
pub struct TuiRequest {
    pub project_root: PathBuf,
}

#[derive(Debug)]
struct TuiModel {
    project: String,
    remote: String,
    sync_status: ProcessStatus,
    processes: Vec<UiProcess>,
    focused: usize,
    logs: Vec<UiLogLine>,
    events: Vec<String>,
    input: String,
    follow_logs: bool,
    log_scroll: u16,
    started_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessStatus {
    Idle,
    Syncing,
    Running,
    Exited(i32),
    Stopped,
    Failed,
    Cancelled,
}

#[derive(Debug)]
struct UiProcess {
    id: u32,
    name: String,
    kind: String,
    status: ProcessStatus,
    command: String,
}

#[derive(Debug)]
struct UiLogLine {
    stream: LogStream,
    text: String,
}

#[derive(Debug)]
enum LogStream {
    Stdout,
    Stderr,
    Rdev,
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ignored = disable_raw_mode();
        let _ignored = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ignored = self.terminal.show_cursor();
    }
}

pub fn run_tui(config: &AppConfig, request: TuiRequest) -> Result<()> {
    let mut guard = init_terminal()?;
    let mut model = TuiModel::prototype(config, request);
    loop {
        guard
            .terminal
            .draw(|frame| draw(frame, &model))
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;

        if event::poll(Duration::from_millis(100))
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?
        {
            let event = event::read()
                .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
            if handle_event(&mut model, event) {
                return Ok(());
            }
        }
    }
}

impl TuiModel {
    fn prototype(config: &AppConfig, request: TuiRequest) -> Self {
        let project = request
            .project_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_owned();
        let remote = format!("{}:{}", config.remote.host, config.remote.path);
        Self {
            project,
            remote,
            sync_status: ProcessStatus::Idle,
            processes: vec![
                UiProcess {
                    id: 0,
                    name: "sync".to_owned(),
                    kind: "watcher".to_owned(),
                    status: ProcessStatus::Syncing,
                    command: "file watcher".to_owned(),
                },
                UiProcess {
                    id: 1,
                    name: "web".to_owned(),
                    kind: "remote".to_owned(),
                    status: ProcessStatus::Running,
                    command: "cd knota-fold && cargo loco start --all".to_owned(),
                },
                UiProcess {
                    id: 2,
                    name: "api".to_owned(),
                    kind: "local".to_owned(),
                    status: ProcessStatus::Failed,
                    command: "cargo run".to_owned(),
                },
                UiProcess {
                    id: 3,
                    name: "build".to_owned(),
                    kind: "local".to_owned(),
                    status: ProcessStatus::Exited(0),
                    command: "npm run build".to_owned(),
                },
            ],
            focused: 1,
            logs: vec![
                UiLogLine::rdev("TUI prototype started. Type quit to leave."),
                UiLogLine::stdout("listening on http://0.0.0.0:5150"),
                UiLogLine::stdout("worker is online"),
                UiLogLine::stdout("scheduler is running"),
                UiLogLine::stderr("example stderr line from focused process"),
            ],
            events: vec![
                "sync idle".to_owned(),
                "remote web running".to_owned(),
                "press ? for help".to_owned(),
            ],
            input: String::new(),
            follow_logs: true,
            log_scroll: 0,
            started_at: Instant::now(),
        }
    }

    fn focused_process(&self) -> Option<&UiProcess> {
        self.processes.get(self.focused)
    }

    fn focus_next(&mut self) {
        if self.processes.is_empty() {
            return;
        }
        self.focused = (self.focused + 1) % self.processes.len();
        self.follow_logs = true;
        self.log_scroll = 0;
        self.events
            .push(format!("focused {}", self.processes[self.focused].name));
    }

    fn focus_prev(&mut self) {
        if self.processes.is_empty() {
            return;
        }
        self.focused = if self.focused == 0 {
            self.processes.len() - 1
        } else {
            self.focused - 1
        };
        self.follow_logs = true;
        self.log_scroll = 0;
        self.events
            .push(format!("focused {}", self.processes[self.focused].name));
    }

    fn push_event(&mut self, event: impl Into<String>) {
        self.events.push(event.into());
        if self.events.len() > 8 {
            let overflow = self.events.len() - 8;
            self.events.drain(0..overflow);
        }
    }
}

impl UiLogLine {
    fn stdout(text: impl Into<String>) -> Self {
        Self {
            stream: LogStream::Stdout,
            text: text.into(),
        }
    }

    fn stderr(text: impl Into<String>) -> Self {
        Self {
            stream: LogStream::Stderr,
            text: text.into(),
        }
    }

    fn rdev(text: impl Into<String>) -> Self {
        Self {
            stream: LogStream::Rdev,
            text: text.into(),
        }
    }
}

impl ProcessStatus {
    fn label(self) -> String {
        match self {
            Self::Idle => "idle".to_owned(),
            Self::Syncing => "syncing".to_owned(),
            Self::Running => "running".to_owned(),
            Self::Exited(code) => format!("exit {code}"),
            Self::Stopped => "stopped".to_owned(),
            Self::Failed => "failed".to_owned(),
            Self::Cancelled => "cancelled".to_owned(),
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Idle | Self::Stopped => Style::default().fg(Color::DarkGray),
            Self::Syncing | Self::Running => Style::default().fg(Color::Green),
            Self::Exited(0) => Style::default().fg(Color::Blue),
            Self::Exited(_) | Self::Failed => Style::default().fg(Color::Red),
            Self::Cancelled => Style::default().fg(Color::Yellow),
        }
    }
}

fn init_terminal() -> Result<TerminalGuard> {
    enable_raw_mode().map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    Ok(TerminalGuard { terminal })
}

fn draw(frame: &mut Frame<'_>, model: &TuiModel) {
    let area = frame.size();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(events_height(area)),
            Constraint::Length(1),
        ])
        .split(area);

    draw_status(frame, vertical[0], model);
    draw_body(frame, vertical[1], model);
    draw_events(frame, vertical[2], model);
    draw_input(frame, vertical[3], model);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let uptime = model.started_at.elapsed().as_secs();
    let focused = model
        .focused_process()
        .map_or("<none>", |process| process.name.as_str());
    let line = Line::from(vec![
        Span::styled(" rdev ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(" project={} ", model.project)),
        Span::raw(format!(" remote={} ", model.remote)),
        Span::styled(
            format!(" sync={} ", model.sync_status.label()),
            model.sync_status.style(),
        ),
        Span::raw(format!(" focus={focused} uptime={}s", uptime)),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Black)),
        area,
    );
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    if area.width < 80 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(4)])
            .split(area);
        draw_logs(frame, chunks[0], model);
        draw_compact_processes(frame, chunks[1], model);
        return;
    }

    let process_width = area
        .width
        .saturating_div(3)
        .clamp(PROCESS_PANEL_MIN_WIDTH, PROCESS_PANEL_MAX_WIDTH);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(process_width)])
        .split(area);
    draw_logs(frame, chunks[0], model);
    draw_process_panel(frame, chunks[1], model);
}

fn draw_logs(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let lines = model
        .logs
        .iter()
        .map(|line| {
            let (label, style) = match line.stream {
                LogStream::Stdout => ("stdout", Style::default().fg(Color::Gray)),
                LogStream::Stderr => ("stderr", Style::default().fg(Color::Red)),
                LogStream::Rdev => ("rdev", Style::default().fg(Color::Yellow)),
            };
            Line::from(vec![
                Span::styled(format!("[{label}] "), style),
                Span::raw(line.text.as_str()),
            ])
        })
        .collect::<Vec<_>>();
    let title = model.focused_process().map_or_else(
        || " Logs ".to_owned(),
        |process| format!(" Logs: {} ", process.name),
    );
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((model.log_scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_process_panel(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(8)])
        .split(area);
    draw_process_list(frame, chunks[0], model);
    draw_process_details(frame, chunks[1], model);
}

fn draw_process_list(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let items = model
        .processes
        .iter()
        .enumerate()
        .map(|(index, process)| {
            let marker = if index == model.focused { "> " } else { "  " };
            let style = if index == model.focused {
                process
                    .status
                    .style()
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                process.status.style()
            };
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(format!("{:<10}", process.name), style),
                Span::styled(process.status.label(), process.status.style()),
            ]))
        })
        .collect::<Vec<_>>();
    let list = List::new(items).block(Block::default().title(" Processes ").borders(Borders::ALL));
    frame.render_widget(list, area);
}

fn draw_process_details(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let lines = model.focused_process().map_or_else(
        || vec![Line::from("no process")],
        |process| {
            vec![
                Line::from(format!("id: {}", process.id)),
                Line::from(format!("name: {}", process.name)),
                Line::from(format!("kind: {}", process.kind)),
                Line::from(format!("status: {}", process.status.label())),
                Line::from("cmd:"),
                Line::from(process.command.as_str()),
            ]
        },
    );
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(" Details ").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_compact_processes(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let spans = model
        .processes
        .iter()
        .enumerate()
        .flat_map(|(index, process)| {
            let marker = if index == model.focused { "> " } else { "" };
            [
                Span::styled(
                    format!("{marker}{}:{} ", process.name, process.status.label()),
                    process.status.style(),
                ),
                Span::raw(" "),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .block(Block::default().title(" Processes ").borders(Borders::ALL)),
        area,
    );
}

fn draw_events(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    if area.height == 0 {
        return;
    }
    let recent = model
        .events
        .iter()
        .rev()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    let text = if recent.is_empty() {
        "Events: <none>".to_owned()
    } else {
        format!("Events: {}", recent.join(" | "))
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let input = format!("{INPUT_PROMPT}{}", model.input);
    frame.render_widget(Paragraph::new(input), area);
}

fn handle_event(model: &mut TuiModel, event: Event) -> bool {
    match event {
        Event::Key(key) => handle_key(model, key),
        Event::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    model.follow_logs = false;
                    model.log_scroll = model.log_scroll.saturating_sub(1);
                }
                MouseEventKind::ScrollDown => {
                    model.log_scroll = model.log_scroll.saturating_add(1);
                }
                MouseEventKind::Down(_) => model.push_event("mouse click received"),
                _ => {}
            }
            false
        }
        Event::Resize(_, _) => false,
        _ => false,
    }
}

fn handle_key(model: &mut TuiModel, key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        model.sync_status = ProcessStatus::Cancelled;
        model.push_event("interrupt requested");
        return false;
    }
    match key.code {
        KeyCode::Char('?') => {
            model.push_event("help: arrows focus, s stop, r restart, type quit to exit");
        }
        KeyCode::Char('s') if model.input.is_empty() => mark_focused(model, ProcessStatus::Stopped),
        KeyCode::Char('r') if model.input.is_empty() => mark_focused(model, ProcessStatus::Running),
        KeyCode::Char('f') if model.input.is_empty() => {
            model.follow_logs = true;
            model.log_scroll = 0;
            model.push_event("log follow enabled");
        }
        KeyCode::Char(ch) => model.input.push(ch),
        KeyCode::Backspace => {
            model.input.pop();
        }
        KeyCode::Enter => return submit_input(model),
        KeyCode::Esc => model.input.clear(),
        KeyCode::Up => model.focus_prev(),
        KeyCode::Down | KeyCode::Tab => model.focus_next(),
        KeyCode::PageUp => {
            model.follow_logs = false;
            model.log_scroll = model.log_scroll.saturating_sub(5);
        }
        KeyCode::PageDown => model.log_scroll = model.log_scroll.saturating_add(5),
        KeyCode::Home => {
            model.follow_logs = false;
            model.log_scroll = 0;
        }
        KeyCode::End => {
            model.follow_logs = true;
            model.log_scroll = 0;
        }
        _ => {}
    }
    false
}

fn submit_input(model: &mut TuiModel) -> bool {
    let command = model.input.trim().to_owned();
    model.input.clear();
    if command.is_empty() {
        return false;
    }
    if command == "quit" || command == "quit!" {
        return true;
    }
    model.push_event(format!("prototype command ignored: {command}"));
    false
}

fn mark_focused(model: &mut TuiModel, status: ProcessStatus) {
    let focused_name = if let Some(process) = model.processes.get_mut(model.focused) {
        process.status = status;
        Some(process.name.clone())
    } else {
        None
    };
    if let Some(name) = focused_name {
        model.push_event(format!("{name} marked {}", status.label()));
    }
}

fn events_height(area: Rect) -> u16 {
    if area.height < 20 {
        0
    } else {
        1
    }
}
