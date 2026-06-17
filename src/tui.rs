use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
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

use notify::{RecursiveMode, Watcher};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::process::SystemProcessRunner;
use crate::session::{
    help_text, parse_console_command, ConsoleCommand, RemoteSessionSpec, SessionManager,
    SharedSessions,
};
use crate::sftp::SftpDeltaBackend;
use crate::sync::{RsyncSyncBackend, SyncBackend, SyncDeltaRequest};
use crate::up::{
    build_watcher, collect_event_changes, reconcile_existing_paths, resolve_local_root,
    sync_backend, EventFilter, PendingChanges, SyncedFiles,
};

const INPUT_PROMPT: &str = "rdev> ";
const PROCESS_PANEL_MIN_WIDTH: u16 = 24;
const PROCESS_PANEL_MAX_WIDTH: u16 = 36;

#[derive(Debug, Clone)]
pub struct TuiRequest {
    pub project_root: PathBuf,
    pub poll: bool,
}

#[derive(Debug)]
struct TuiModel {
    config: AppConfig,
    sessions: SharedSessions,
    project: String,
    remote: String,
    sync_status: ProcessStatus,
    processes: Vec<UiProcess>,
    focused: usize,
    logs: Vec<UiLogLine>,
    sync_logs: Vec<UiLogLine>,
    events: Vec<String>,
    input: String,
    follow_logs: bool,
    log_scroll: u16,
    started_at: Instant,
}

struct TuiSyncRuntime {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Event>,
    pending: PendingChanges,
    synced_files: SyncedFiles,
    last_event_at: Instant,
    local_root: PathBuf,
    debounce: Duration,
}

struct TuiSyncContext<'a> {
    config: &'a AppConfig,
    backend: &'a dyn SyncBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessStatus {
    Idle,
    Running,
    Exited(i32),
    Stopped,
    Failed,
    Cancelled,
}

#[derive(Debug)]
struct UiProcess {
    id: u32,
    session_id: Option<u32>,
    name: String,
    kind: String,
    status: ProcessStatus,
    command: String,
}

#[derive(Debug, Clone)]
struct UiLogLine {
    stream: LogStream,
    text: String,
}

#[derive(Debug, Clone)]
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
    let runner = SystemProcessRunner::default();
    let rsync_backend = RsyncSyncBackend::new(config, &runner);
    let ssh_backend = SftpDeltaBackend::new(config);
    let backend = sync_backend(config, &rsync_backend, &ssh_backend);
    let mut sync = TuiSyncRuntime::new(config, &request)?;
    let mut guard = init_terminal()?;
    let mut model = TuiModel::new(config, request);
    loop {
        sync.process_events(TuiSyncContext { config, backend }, &mut model);
        model.refresh_sessions();
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
                if let Ok(mut manager) = model.sessions.lock() {
                    manager.stop_all();
                }
                return Ok(());
            }
        }
    }
}

impl TuiModel {
    fn new(config: &AppConfig, request: TuiRequest) -> Self {
        let project = request
            .project_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_owned();
        let remote = format!("{}:{}", config.remote.host, config.remote.path);
        Self {
            config: config.clone(),
            sessions: SessionManager::shared(request.project_root),
            project,
            remote,
            sync_status: ProcessStatus::Idle,
            processes: Vec::new(),
            focused: 0,
            logs: vec![
                UiLogLine::rdev("TUI session mode started. Type help for commands."),
                UiLogLine::rdev("Focus a session to view its logs."),
            ],
            sync_logs: vec![
                UiLogLine::rdev("sync watcher started"),
                UiLogLine::rdev("initial full sync is not run in TUI yet"),
            ],
            events: vec![
                "sync watcher started".to_owned(),
                "real sessions enabled".to_owned(),
                "type new session <name> -- <command>".to_owned(),
            ],
            input: String::new(),
            follow_logs: true,
            log_scroll: 0,
            started_at: Instant::now(),
        }
    }

    fn refresh_sessions(&mut self) {
        let previous_session = self
            .processes
            .get(self.focused)
            .and_then(|process| process.session_id);
        let snapshot = if let Ok(mut manager) = self.sessions.lock() {
            Some(manager.snapshot())
        } else {
            None
        };
        let Some(snapshot) = snapshot else {
            self.push_event("session manager unavailable");
            return;
        };
        let mut processes = vec![UiProcess {
            id: 0,
            session_id: None,
            name: "sync".to_owned(),
            kind: "watcher".to_owned(),
            status: self.sync_status,
            command: "file watcher".to_owned(),
        }];
        processes.extend(snapshot.sessions.iter().map(|session| UiProcess {
            id: session.id,
            session_id: Some(session.id),
            name: session.name.clone(),
            kind: session.kind.clone(),
            status: ProcessStatus::from_session(session.status.as_str(), session.exit_code),
            command: session.command.clone(),
        }));
        let focused_session = previous_session.or(snapshot.focused);
        self.focused = focused_session
            .and_then(|id| {
                processes
                    .iter()
                    .position(|process| process.session_id == Some(id))
            })
            .unwrap_or_else(|| self.focused.min(processes.len().saturating_sub(1)));
        self.logs = if let Some(session_id) = processes.get(self.focused).and_then(|p| p.session_id)
        {
            snapshot
                .sessions
                .iter()
                .find(|session| session.id == session_id)
                .map(|session| {
                    session
                        .logs
                        .iter()
                        .map(|line| parse_log_line(line))
                        .collect()
                })
                .unwrap_or_else(|| vec![UiLogLine::rdev("logs: <empty>")])
        } else {
            self.sync_logs.clone()
        };
        if self.logs.is_empty() {
            self.logs.push(UiLogLine::rdev("logs: <empty>"));
        }
        self.processes = processes;
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
        self.focus_manager_to_current();
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
        self.focus_manager_to_current();
    }

    fn push_event(&mut self, event: impl Into<String>) {
        self.events.push(event.into());
        if self.events.len() > 8 {
            let overflow = self.events.len() - 8;
            self.events.drain(0..overflow);
        }
    }

    fn push_sync_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.sync_logs.push(UiLogLine::rdev(line.clone()));
        self.push_event(line);
        if self.sync_logs.len() > 500 {
            let overflow = self.sync_logs.len() - 500;
            self.sync_logs.drain(0..overflow);
        }
    }

    fn focus_manager_to_current(&mut self) {
        let Some(session_id) = self
            .processes
            .get(self.focused)
            .and_then(|process| process.session_id)
        else {
            return;
        };
        if let Ok(mut manager) = self.sessions.lock() {
            let _ignored = manager.focus(&session_id.to_string());
        }
    }
}

impl TuiSyncRuntime {
    fn new(config: &AppConfig, request: &TuiRequest) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let mut watcher = build_watcher(request.poll, sender)?;
        let local_root = resolve_local_root(&request.project_root, &config.sync.local_path);
        for watch_dir in &config.sync.watch_dirs {
            let watch_path = local_root.join(watch_dir);
            watcher
                .watch(&watch_path, RecursiveMode::Recursive)
                .map_err(|source| {
                    err_with_source(error_info::WATCH_EVENT_FAILED, source)
                        .with_path(watch_path.display())
                })?;
        }
        Ok(Self {
            _watcher: watcher,
            receiver,
            pending: PendingChanges::default(),
            synced_files: SyncedFiles::default(),
            last_event_at: Instant::now(),
            local_root,
            debounce: Duration::from_millis(config.sync.debounce_ms.max(50)),
        })
    }

    fn process_events(&mut self, context: TuiSyncContext<'_>, model: &mut TuiModel) {
        while let Ok(event) = self.receiver.try_recv() {
            let filter = EventFilter {
                local_root: &self.local_root,
                watch_dirs: &context.config.sync.watch_dirs,
                excludes: &context.config.sync.exclude,
            };
            if let Some(changes) = collect_event_changes(&event, &filter) {
                self.pending.merge(changes);
                self.last_event_at = Instant::now();
                model.sync_status = ProcessStatus::Running;
                model.push_sync_log("file change detected");
            }
        }
        if !self.pending.has_changes() || self.last_event_at.elapsed() < self.debounce {
            if model.sync_status == ProcessStatus::Running {
                model.sync_status = ProcessStatus::Idle;
            }
            return;
        }
        let changes = self.pending.take();
        let changes = reconcile_existing_paths(changes, &self.local_root);
        let changes = self.synced_files.filter_changed(changes, &self.local_root);
        if !changes.has_changes() {
            model.sync_status = ProcessStatus::Idle;
            return;
        }
        model.sync_status = ProcessStatus::Running;
        let upload_count = changes.uploads.len();
        let delete_count = changes.deletes.len();
        model.push_sync_log(format!(
            "delta start uploads={upload_count} deletes={delete_count}"
        ));
        let result = context.backend.sync_delta(SyncDeltaRequest {
            project_root: self.local_root.clone(),
            uploads: changes.uploads.iter().cloned().collect(),
            deletes: changes.deletes.iter().cloned().collect(),
            cancelled: None,
        });
        match result {
            Ok(report) => {
                self.synced_files.record(&changes, &self.local_root);
                model.sync_status = ProcessStatus::Idle;
                model.push_sync_log(report.format_text());
            }
            Err(error) => {
                model.sync_status = ProcessStatus::Failed;
                model.push_sync_log(error.to_string());
            }
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
    fn from_session(status: &str, exit_code: Option<i32>) -> Self {
        match status {
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            "exited" => exit_code.map_or(Self::Exited(0), Self::Exited),
            _ => Self::Failed,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Idle => "idle".to_owned(),
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
            Self::Running => Style::default().fg(Color::Green),
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
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        model.sync_status = ProcessStatus::Cancelled;
        model.push_event("interrupt requested");
        return false;
    }
    match key.code {
        KeyCode::Char('?') => {
            model.push_event("help: arrows focus, s stop, r restart, type quit to exit");
        }
        KeyCode::Char('s') if model.input.is_empty() => stop_focused(model),
        KeyCode::Char('r') if model.input.is_empty() => restart_focused(model),
        KeyCode::Char('f') if model.input.is_empty() => {
            model.follow_logs = true;
            model.log_scroll = 0;
            model.push_event("log follow enabled");
        }
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_digit() =>
        {
            focus_by_digit(model, ch);
        }
        KeyCode::Char(ch) => model.input.push(ch),
        KeyCode::Backspace => {
            model.input.pop();
        }
        KeyCode::Enter => return submit_input(model),
        KeyCode::Esc => model.input.clear(),
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => model.focus_prev(),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => model.focus_next(),
        KeyCode::Up | KeyCode::Down => {}
        KeyCode::Tab => model.focus_next(),
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
    execute_console_command(model, parse_console_command(&command))
}

fn focus_by_digit(model: &mut TuiModel, digit: char) {
    let Some(index) = digit.to_digit(10) else {
        return;
    };
    if index == 0 {
        return;
    }
    let index = index as usize - 1;
    if index >= model.processes.len() {
        return;
    }
    model.focused = index;
    model.follow_logs = true;
    model.log_scroll = 0;
    model
        .events
        .push(format!("focused {}", model.processes[model.focused].name));
    model.focus_manager_to_current();
}

fn execute_console_command(model: &mut TuiModel, command: ConsoleCommand) -> bool {
    let result = match command {
        ConsoleCommand::Help => Ok(help_text().to_owned()),
        ConsoleCommand::Sessions => lock_sessions(model).map(|mut manager| manager.list()),
        ConsoleCommand::NewSession { name, command } => {
            SessionManager::start(&model.sessions, name, command)
        }
        ConsoleCommand::NewRemoteSession { name, command } => {
            RemoteSessionSpec::from_config(&model.config, name, command)
                .and_then(|spec| SessionManager::start_remote(&model.sessions, spec))
        }
        ConsoleCommand::Logs { selector } => {
            lock_sessions(model).and_then(|mut manager| manager.logs(selector.as_deref()))
        }
        ConsoleCommand::Tail { selector, lines } => lock_sessions(model)
            .and_then(|mut manager| manager.tail_logs(selector.as_deref(), lines)),
        ConsoleCommand::ClearLogs { selector } => {
            lock_sessions(model).and_then(|mut manager| manager.clear_logs(selector.as_deref()))
        }
        ConsoleCommand::Focus { selector } => {
            lock_sessions(model).and_then(|mut manager| manager.focus(&selector))
        }
        ConsoleCommand::Stop { selector } => {
            lock_sessions(model).and_then(|mut manager| manager.stop(&selector))
        }
        ConsoleCommand::Restart { selector } => SessionManager::restart(&model.sessions, &selector),
        ConsoleCommand::Sync => {
            model.sync_status = ProcessStatus::Cancelled;
            Ok("sync is not wired into TUI yet".to_owned())
        }
        ConsoleCommand::Quit => {
            let running = lock_sessions(model)
                .map(|mut manager| manager.has_running())
                .unwrap_or(false);
            if running {
                Ok("running sessions exist; use quit! to stop sessions and exit".to_owned())
            } else {
                return true;
            }
        }
        ConsoleCommand::QuitForce => {
            if let Ok(mut manager) = lock_sessions(model) {
                manager.stop_all();
            }
            return true;
        }
        ConsoleCommand::Empty => Ok(String::new()),
        ConsoleCommand::Unknown(message) => Ok(format!("unknown command: {message}")),
    };
    match result {
        Ok(message) => {
            for line in message.lines().filter(|line| !line.is_empty()) {
                model.push_event(line.to_owned());
            }
        }
        Err(error) => model.push_event(error.to_string()),
    }
    model.refresh_sessions();
    false
}

fn stop_focused(model: &mut TuiModel) {
    let Some(selector) = focused_session_selector(model) else {
        model.push_event("sync watcher cannot be stopped in TUI yet");
        return;
    };
    let result = lock_sessions(model).and_then(|mut manager| manager.stop(&selector));
    push_result_event(model, result);
    model.refresh_sessions();
}

fn restart_focused(model: &mut TuiModel) {
    let Some(selector) = focused_session_selector(model) else {
        model.push_event("sync watcher cannot be restarted in TUI yet");
        return;
    };
    let result = SessionManager::restart(&model.sessions, &selector);
    push_result_event(model, result);
    model.refresh_sessions();
}

fn focused_session_selector(model: &TuiModel) -> Option<String> {
    model
        .processes
        .get(model.focused)
        .and_then(|process| process.session_id)
        .map(|id| id.to_string())
}

fn lock_sessions(
    model: &TuiModel,
) -> Result<std::sync::MutexGuard<'_, crate::session::SessionManager>> {
    model
        .sessions
        .lock()
        .map_err(|_| err(error_info::WATCH_EVENT_FAILED).with_hint("session manager poisoned"))
}

fn push_result_event(model: &mut TuiModel, result: Result<String>) {
    match result {
        Ok(message) => model.push_event(message),
        Err(error) => model.push_event(error.to_string()),
    }
}

fn parse_log_line(line: &str) -> UiLogLine {
    if let Some(text) = line.strip_prefix("[stdout] ") {
        UiLogLine::stdout(text)
    } else if let Some(text) = line.strip_prefix("[stderr] ") {
        UiLogLine::stderr(text)
    } else {
        UiLogLine::rdev(line)
    }
}

fn events_height(area: Rect) -> u16 {
    if area.height < 20 {
        0
    } else {
        1
    }
}
