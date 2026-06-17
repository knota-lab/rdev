use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use notify::{RecursiveMode, Watcher};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, RdevError, Result};
use crate::error_info;
use crate::process::SystemProcessRunner;
use crate::session::{
    help_text, parse_console_command, ConsoleCommand, RemoteSessionSpec, SessionManager,
    SharedSessions,
};
use crate::sftp::SftpDeltaBackend;
use crate::sync::{RsyncSyncBackend, SyncDeltaRequest};
use crate::sync_output::SyncOutput;
use crate::up::{
    build_watcher, collect_event_changes, reconcile_existing_paths, resolve_local_root,
    sync_backend, EventFilter, PendingChanges, SyncedFiles,
};

mod input;
mod logs;
mod state;

use self::input::{
    next_char_boundary, next_word_boundary, previous_char_boundary, previous_word_boundary,
};
use self::logs::{parse_log_line, selected_line, wrapped_log_rows, UiLogLine};
use self::state::{SavedSession, SavedSessionKind, TuiStateStore};

const INPUT_PROMPT: &str = "rdev> ";
const PROCESS_PANEL_MIN_WIDTH: u16 = 24;
const PROCESS_PANEL_MAX_WIDTH: u16 = 36;
const EVENT_POLL: Duration = Duration::from_millis(100);
const COMMAND_HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub struct TuiRequest {
    pub project_root: PathBuf,
    pub poll: bool,
}

#[derive(Debug)]
struct TuiModel {
    config: AppConfig,
    state: TuiStateStore,
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
    input_cursor: usize,
    command_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    follow_logs: bool,
    log_scroll: u16,
    log_region: Option<LogRegion>,
    selection: Option<TextSelection>,
}

struct TuiSyncRuntime {
    _watcher: notify::RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Event>,
    worker: TuiSyncWorker,
    pending: PendingChanges,
    synced_files: SyncedFiles,
    last_event_at: Instant,
    local_root: PathBuf,
    debounce: Duration,
}

struct TuiSyncWorker {
    sender: mpsc::Sender<SyncJob>,
    receiver: mpsc::Receiver<SyncWorkerEvent>,
    in_flight: bool,
    cancel: Option<Arc<AtomicBool>>,
}

struct SyncJob {
    project_root: PathBuf,
    changes: PendingChanges,
    cancel: Arc<AtomicBool>,
}

enum SyncWorkerEvent {
    Output(String),
    Done {
        changes: PendingChanges,
        cancelled: bool,
        result: std::result::Result<String, String>,
    },
}

#[derive(Clone)]
struct ChannelSyncOutput {
    sender: mpsc::Sender<SyncWorkerEvent>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiProcess {
    id: u32,
    session_id: Option<u32>,
    name: String,
    kind: String,
    status: ProcessStatus,
    command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogRegion {
    content: Rect,
    first_row: usize,
    rows: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextSelection {
    anchor: CellPos,
    cursor: CellPos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellPos {
    row: usize,
    col: u16,
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ignored = disable_raw_mode();
        let _ignored = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ignored = self.terminal.show_cursor();
    }
}

pub fn run_tui(config: &AppConfig, request: TuiRequest) -> Result<()> {
    let mut sync = TuiSyncRuntime::new(config, &request)?;
    let mut guard = init_terminal()?;
    let mut model = TuiModel::new(config, request);
    let mut dirty = true;
    loop {
        dirty |= sync.process_events(config, &mut model);
        dirty |= model.refresh_sessions();
        if dirty {
            guard
                .terminal
                .draw(|frame| draw(frame, &mut model))
                .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
            dirty = false;
        }

        if event::poll(EVENT_POLL)
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?
        {
            let event = event::read()
                .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
            if handle_event(&mut model, &mut sync, event) {
                if let Ok(mut manager) = model.sessions.lock() {
                    manager.stop_all();
                }
                return Ok(());
            }
            dirty = true;
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
        let (state, state_event) = TuiStateStore::load(&request.project_root);
        let mut events = vec![
            "sync watcher started".to_owned(),
            "real sessions enabled".to_owned(),
            "type new session <name> -- <command>".to_owned(),
        ];
        if let Some(event) = state_event {
            events.push(event);
        }
        Self {
            config: config.clone(),
            command_history: state.command_history().to_vec(),
            state,
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
            events,
            input: String::new(),
            input_cursor: 0,
            history_index: None,
            history_draft: String::new(),
            follow_logs: true,
            log_scroll: 0,
            log_region: None,
            selection: None,
        }
    }

    fn refresh_sessions(&mut self) -> bool {
        let previous_session = self
            .processes
            .get(self.focused)
            .and_then(|process| process.session_id);
        let previous_was_sync = self
            .processes
            .get(self.focused)
            .is_some_and(|process| process.session_id.is_none());
        let snapshot = if let Ok(mut manager) = self.sessions.lock() {
            Some(manager.snapshot())
        } else {
            None
        };
        let Some(snapshot) = snapshot else {
            self.push_event("session manager unavailable");
            return true;
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
        let focused = if previous_was_sync {
            0
        } else {
            let focused_session = previous_session.or(snapshot.focused);
            focused_session
                .and_then(|id| {
                    processes
                        .iter()
                        .position(|process| process.session_id == Some(id))
                })
                .unwrap_or_else(|| self.focused.min(processes.len().saturating_sub(1)))
        };
        let mut logs = if let Some(session_id) = processes.get(focused).and_then(|p| p.session_id) {
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
        if logs.is_empty() {
            logs.push(UiLogLine::rdev("logs: <empty>"));
        }
        let dirty = self.focused != focused || self.processes != processes || self.logs != logs;
        self.focused = focused;
        self.processes = processes;
        self.logs = logs;
        if dirty {
            self.selection = None;
        }
        dirty
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
        self.selection = None;
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
        self.selection = None;
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

    fn insert_char(&mut self, ch: char) {
        self.exit_history_browse();
        self.input.insert(self.input_cursor, ch);
        self.input_cursor += ch.len_utf8();
    }

    fn insert_text(&mut self, text: &str) {
        self.exit_history_browse();
        self.input.insert_str(self.input_cursor, text);
        self.input_cursor += text.len();
    }

    fn backspace_input(&mut self) {
        self.exit_history_browse();
        if self.input_cursor == 0 {
            return;
        }
        let previous = previous_char_boundary(&self.input, self.input_cursor);
        self.input.drain(previous..self.input_cursor);
        self.input_cursor = previous;
    }

    fn delete_input(&mut self) {
        self.exit_history_browse();
        if self.input_cursor >= self.input.len() {
            return;
        }
        let next = next_char_boundary(&self.input, self.input_cursor);
        self.input.drain(self.input_cursor..next);
    }

    fn backspace_word_input(&mut self) {
        self.exit_history_browse();
        let previous = previous_word_boundary(&self.input, self.input_cursor);
        self.input.drain(previous..self.input_cursor);
        self.input_cursor = previous;
    }

    fn delete_word_input(&mut self) {
        self.exit_history_browse();
        let next = next_word_boundary(&self.input, self.input_cursor);
        self.input.drain(self.input_cursor..next);
    }

    fn move_input_left(&mut self) {
        self.input_cursor = previous_char_boundary(&self.input, self.input_cursor);
    }

    fn move_input_right(&mut self) {
        self.input_cursor = next_char_boundary(&self.input, self.input_cursor);
    }

    fn move_input_word_left(&mut self) {
        self.input_cursor = previous_word_boundary(&self.input, self.input_cursor);
    }

    fn move_input_word_right(&mut self) {
        self.input_cursor = next_word_boundary(&self.input, self.input_cursor);
    }

    fn move_input_home(&mut self) {
        self.input_cursor = 0;
    }

    fn move_input_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.history_index = None;
        self.history_draft.clear();
    }

    fn push_command_history(&mut self, command: &str) {
        if command.is_empty() {
            return;
        }
        match self
            .state
            .push_command_history(command, COMMAND_HISTORY_LIMIT)
        {
            Ok(()) => {
                self.command_history = self.state.command_history().to_vec();
            }
            Err(error) => self.push_event(error.to_string()),
        }
    }

    fn history_prev(&mut self) {
        if self.command_history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = self.input.clone();
                self.command_history.len() - 1
            }
        };
        self.history_index = Some(index);
        self.input = self.command_history[index].clone();
        self.input_cursor = self.input.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.command_history.len() {
            let next = index + 1;
            self.history_index = Some(next);
            self.input = self.command_history[next].clone();
        } else {
            self.history_index = None;
            self.input = std::mem::take(&mut self.history_draft);
        }
        self.input_cursor = self.input.len();
    }

    fn exit_history_browse(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
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
            worker: TuiSyncWorker::spawn(config.clone()),
            pending: PendingChanges::default(),
            synced_files: SyncedFiles::default(),
            last_event_at: Instant::now(),
            local_root,
            debounce: Duration::from_millis(config.sync.debounce_ms.max(50)),
        })
    }

    fn process_events(&mut self, config: &AppConfig, model: &mut TuiModel) -> bool {
        let mut dirty = false;
        while let Ok(event) = self.worker.receiver.try_recv() {
            dirty = true;
            match event {
                SyncWorkerEvent::Output(line) => model.push_sync_log(line),
                SyncWorkerEvent::Done {
                    changes,
                    cancelled,
                    result,
                } => {
                    self.worker.in_flight = false;
                    self.worker.cancel = None;
                    match result {
                        Ok(message) => {
                            self.synced_files.record(&changes, &self.local_root);
                            model.push_sync_log(message);
                            model.sync_status = if self.pending.has_changes() {
                                ProcessStatus::Running
                            } else {
                                ProcessStatus::Idle
                            };
                        }
                        Err(error) => {
                            if cancelled {
                                model.sync_status = if self.pending.has_changes() {
                                    ProcessStatus::Running
                                } else {
                                    ProcessStatus::Cancelled
                                };
                                model.push_sync_log("sync cancelled");
                            } else {
                                model.sync_status = ProcessStatus::Failed;
                                model.push_sync_log(error);
                            }
                        }
                    }
                }
            }
        }
        while let Ok(event) = self.receiver.try_recv() {
            let filter = EventFilter {
                local_root: &self.local_root,
                watch_dirs: &config.sync.watch_dirs,
                excludes: &config.sync.exclude,
            };
            if let Some(changes) = collect_event_changes(&event, &filter) {
                self.pending.merge(changes);
                self.last_event_at = Instant::now();
                model.sync_status = ProcessStatus::Running;
                model.push_sync_log("file change detected");
                dirty = true;
            }
        }
        if self.worker.in_flight {
            model.sync_status = ProcessStatus::Running;
            return dirty;
        }
        if !self.pending.has_changes() {
            if model.sync_status == ProcessStatus::Running {
                model.sync_status = ProcessStatus::Idle;
                dirty = true;
            }
            return dirty;
        }
        if self.last_event_at.elapsed() < self.debounce {
            return dirty;
        }
        let changes = self.pending.take();
        let changes = reconcile_existing_paths(changes, &self.local_root);
        let changes = self.synced_files.filter_changed(changes, &self.local_root);
        if !changes.has_changes() {
            model.sync_status = ProcessStatus::Idle;
            return true;
        }
        model.sync_status = ProcessStatus::Running;
        let upload_count = changes.uploads.len();
        let delete_count = changes.deletes.len();
        model.push_sync_log(format!(
            "delta start uploads={upload_count} deletes={delete_count}"
        ));
        let cancel = Arc::new(AtomicBool::new(false));
        match self.worker.sender.send(SyncJob {
            project_root: self.local_root.clone(),
            changes,
            cancel: Arc::clone(&cancel),
        }) {
            Ok(()) => {
                self.worker.in_flight = true;
                self.worker.cancel = Some(cancel);
            }
            Err(error) => {
                model.sync_status = ProcessStatus::Failed;
                model.push_sync_log(format!("sync worker stopped: {error}"));
            }
        }
        true
    }

    fn cancel_current(&mut self, model: &mut TuiModel) -> bool {
        if let Some(cancel) = &self.worker.cancel {
            cancel.store(true, Ordering::SeqCst);
            model.sync_status = ProcessStatus::Cancelled;
            model.push_sync_log("sync cancellation requested");
            return true;
        }
        if self.pending.has_changes() {
            self.pending = PendingChanges::default();
            model.sync_status = ProcessStatus::Cancelled;
            model.push_sync_log("pending sync cancelled");
            return true;
        }
        false
    }
}

impl TuiSyncWorker {
    fn spawn(config: AppConfig) -> Self {
        let (job_sender, job_receiver) = mpsc::channel::<SyncJob>();
        let (event_sender, event_receiver) = mpsc::channel::<SyncWorkerEvent>();
        thread::spawn({
            let event_sender = event_sender.clone();
            move || {
                let runner = SystemProcessRunner::default();
                let rsync_backend = RsyncSyncBackend::new(&config, &runner);
                let ssh_backend =
                    SftpDeltaBackend::new(&config).with_output(Arc::new(ChannelSyncOutput {
                        sender: event_sender.clone(),
                    }));
                let backend = sync_backend(&config, &rsync_backend, &ssh_backend);
                while let Ok(job) = job_receiver.recv() {
                    let request = SyncDeltaRequest {
                        project_root: job.project_root,
                        uploads: job.changes.uploads.iter().cloned().collect(),
                        deletes: job.changes.deletes.iter().cloned().collect(),
                        cancelled: Some(Arc::clone(&job.cancel)),
                    };
                    let result = backend
                        .sync_delta(request)
                        .map(|report| report.format_text())
                        .map_err(|error| error.to_string());
                    let _send_result = event_sender.send(SyncWorkerEvent::Done {
                        changes: job.changes,
                        cancelled: job.cancel.load(Ordering::SeqCst),
                        result,
                    });
                }
            }
        });
        Self {
            sender: job_sender,
            receiver: event_receiver,
            in_flight: false,
            cancel: None,
        }
    }
}

impl SyncOutput for ChannelSyncOutput {
    fn line(&self, line: String) {
        let _send_result = self.sender.send(SyncWorkerEvent::Output(line));
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )
    .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    Ok(TerminalGuard { terminal })
}

fn draw(frame: &mut Frame<'_>, model: &mut TuiModel) {
    let area = frame.size();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(events_height(area)),
            Constraint::Length(3),
        ])
        .split(area);

    draw_status(frame, vertical[0], model);
    draw_body(frame, vertical[1], model);
    draw_events(frame, vertical[2], model);
    draw_input(frame, vertical[3], model);
    set_input_cursor(frame, vertical[3], model);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
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
        Span::raw(format!(" focus={focused}")),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Black)),
        area,
    );
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, model: &mut TuiModel) {
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

fn draw_logs(frame: &mut Frame<'_>, area: Rect, model: &mut TuiModel) {
    let title = model.focused_process().map_or_else(
        || " Logs ".to_owned(),
        |process| format!(" Logs: {} ", process.name),
    );
    frame.render_widget(Block::default().title(title).borders(Borders::ALL), area);

    let content = log_content_area(area);
    let all_rows = wrapped_log_rows(&model.logs, content.width);
    let scroll = clamped_visual_log_scroll(all_rows.len(), content.height, model.log_scroll);
    let rows = all_rows
        .into_iter()
        .skip(scroll as usize)
        .take(content.height as usize)
        .collect::<Vec<_>>();
    model.log_region = Some(LogRegion {
        content,
        first_row: scroll as usize,
        rows: rows.iter().map(|row| row.plain.clone()).collect(),
    });
    let lines = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let absolute_row = scroll as usize + index;
            selected_line(row, absolute_row, model.selection)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), content);
}

fn log_content_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn clamped_visual_log_scroll(row_count: usize, visible_rows: u16, scroll: u16) -> u16 {
    if visible_rows == 0 {
        return 0;
    }
    let max_scroll = (row_count as u16).saturating_sub(visible_rows);
    scroll.min(max_scroll)
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
    let input_area = input_text_area(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(36, 36, 36))),
        area,
    );
    let line = Line::from(vec![
        Span::styled(
            INPUT_PROMPT,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(model.input.as_str(), Style::default().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(line), input_area);
}

fn set_input_cursor(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let input_area = input_text_area(area);
    if input_area.height == 0 || input_area.width == 0 {
        return;
    }
    let prompt_width = UnicodeWidthStr::width(INPUT_PROMPT) as u16;
    let input_width = UnicodeWidthStr::width(&model.input[..model.input_cursor]) as u16;
    let max_x = input_area.width.saturating_sub(1);
    let cursor_x = prompt_width.saturating_add(input_width).min(max_x);
    frame.set_cursor(input_area.x.saturating_add(cursor_x), input_area.y);
}

fn input_text_area(area: Rect) -> Rect {
    if area.height >= 3 {
        Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(1),
            height: 1,
        }
    } else {
        area
    }
}

fn handle_event(model: &mut TuiModel, sync: &mut TuiSyncRuntime, event: Event) -> bool {
    match event {
        Event::Key(key) => handle_key(model, sync, key),
        Event::Paste(text) => {
            model.insert_text(&text);
            false
        }
        Event::Mouse(mouse) => {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    model.selection =
                        selection_pos(model, mouse.column, mouse.row).map(|pos| TextSelection {
                            anchor: pos,
                            cursor: pos,
                        });
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(pos) = selection_pos(model, mouse.column, mouse.row) {
                        if let Some(selection) = model.selection.as_mut() {
                            selection.cursor = pos;
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(pos) = selection_pos(model, mouse.column, mouse.row) {
                        if let Some(selection) = model.selection.as_mut() {
                            selection.cursor = pos;
                        }
                    }
                }
                MouseEventKind::ScrollUp => {
                    model.follow_logs = false;
                    model.log_scroll = model.log_scroll.saturating_add(1);
                    model.clamp_log_scroll();
                    model.selection = None;
                }
                MouseEventKind::ScrollDown => {
                    model.log_scroll = model.log_scroll.saturating_sub(1);
                    model.clamp_log_scroll();
                    model.selection = None;
                }
                _ => {}
            }
            false
        }
        Event::Resize(_, _) => false,
        _ => false,
    }
}

fn handle_key(model: &mut TuiModel, sync: &mut TuiSyncRuntime, key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if copy_selection(model) {
            return false;
        }
        if focused_process_is_sync(model) && sync.cancel_current(model) {
            return false;
        }
        if focused_process_is_sync(model) {
            model.sync_status = ProcessStatus::Cancelled;
            model.push_event("sync cancel requested");
        } else {
            model.push_event("ctrl+c copies selection; focus sync to cancel sync");
        }
        return false;
    }
    match key.code {
        KeyCode::Char('?') => model.push_event(
            "help: drag logs to select, ctrl+c copies selection, ctrl+arrows focus, quit",
        ),
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
        KeyCode::Char(ch) => model.insert_char(ch),
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.backspace_word_input();
        }
        KeyCode::Backspace => model.backspace_input(),
        KeyCode::Delete if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.delete_word_input();
        }
        KeyCode::Delete => model.delete_input(),
        KeyCode::Enter => return submit_input(model),
        KeyCode::Esc => {
            model.clear_input();
            model.selection = None;
        }
        KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => model.focus_prev(),
        KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => model.focus_next(),
        KeyCode::Up => model.history_prev(),
        KeyCode::Down => model.history_next(),
        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.move_input_word_left();
        }
        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
            model.move_input_word_right();
        }
        KeyCode::Left => model.move_input_left(),
        KeyCode::Right => model.move_input_right(),
        KeyCode::Tab => model.focus_next(),
        KeyCode::PageUp if model.input.is_empty() => {
            model.follow_logs = false;
            model.log_scroll = model.log_scroll.saturating_add(5);
            model.clamp_log_scroll();
        }
        KeyCode::PageDown if model.input.is_empty() => {
            model.log_scroll = model.log_scroll.saturating_sub(5);
            model.clamp_log_scroll();
        }
        KeyCode::Home if !model.input.is_empty() => model.move_input_home(),
        KeyCode::End if !model.input.is_empty() => model.move_input_end(),
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

impl TuiModel {
    fn clamp_log_scroll(&mut self) {
        self.log_scroll = self
            .log_scroll
            .min(self.logs.len().saturating_sub(1) as u16);
    }
}

fn selection_pos(model: &TuiModel, x: u16, y: u16) -> Option<CellPos> {
    let region = model.log_region.as_ref()?;
    if !contains(region.content, x, y) || region.rows.is_empty() {
        return None;
    }
    let visible_row = y.saturating_sub(region.content.y) as usize;
    let row = region.first_row.saturating_add(visible_row);
    let row_text = region.rows.get(visible_row)?;
    let max_col = UnicodeWidthStr::width(row_text.as_str()) as u16;
    let col = x.saturating_sub(region.content.x).min(max_col);
    Some(CellPos { row, col })
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn copy_selection(model: &mut TuiModel) -> bool {
    let Some(text) = selected_text(model) else {
        return false;
    };
    if text.is_empty() {
        model.selection = None;
        return false;
    }
    match write_clipboard(&text) {
        Ok(()) => {
            model.push_event(format!("copied {} lines", text.lines().count()));
            model.selection = None;
        }
        Err(error) => model.push_event(tui_error_message(&error)),
    }
    true
}

fn selected_text(model: &TuiModel) -> Option<String> {
    let selection = model.selection?;
    let region = model.log_region.as_ref()?;
    let (start, end) = ordered_selection(selection);
    let mut lines = Vec::new();
    for row in start.row..=end.row {
        let visible_row = row.checked_sub(region.first_row)?;
        let text = region.rows.get(visible_row)?;
        let width = UnicodeWidthStr::width(text.as_str()) as u16;
        let start_col = if row == start.row {
            start.col.min(width)
        } else {
            0
        };
        let end_col = if row == end.row {
            end.col.min(width)
        } else {
            width
        };
        if start_col > end_col {
            return None;
        }
        let start_index = byte_index_for_display_col(text, start_col);
        let end_index = byte_index_for_display_col(text, end_col);
        lines.push(text[start_index..end_index].to_owned());
    }
    Some(lines.join("\n"))
}

fn ordered_selection(selection: TextSelection) -> (CellPos, CellPos) {
    if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    }
}

fn byte_index_for_display_col(text: &str, col: u16) -> usize {
    let mut width = 0;
    for (index, ch) in text.char_indices() {
        let next_width = width + ch.width().unwrap_or(0) as u16;
        if next_width > col {
            return index;
        }
        width = next_width;
    }
    text.len()
}

fn submit_input(model: &mut TuiModel) -> bool {
    let command = model.input.trim().to_owned();
    model.clear_input();
    if command.is_empty() {
        return false;
    }
    model.push_command_history(&command);
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
            start_and_remember_local(model, name, command)
        }
        ConsoleCommand::NewRemoteSession { name, command } => {
            start_and_remember_remote(model, name, command)
        }
        ConsoleCommand::SavedSessions => Ok(model.state.saved_sessions_text()),
        ConsoleCommand::RestoreSession { selector } => restore_saved_session(model, &selector),
        ConsoleCommand::DeleteSavedSession { selector } => model.state.delete_session(&selector),
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
        ConsoleCommand::StopFocused => {
            stop_focused(model);
            Ok(String::new())
        }
        ConsoleCommand::Restart { selector } => SessionManager::restart(&model.sessions, &selector),
        ConsoleCommand::RestartFocused => {
            restart_focused(model);
            Ok(String::new())
        }
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
        Err(error) => model.push_event(tui_error_message(&error)),
    }
    model.refresh_sessions();
    false
}

fn restore_saved_session(model: &mut TuiModel, selector: &str) -> Result<String> {
    let Some(saved) = model.state.find_session(selector) else {
        return Err(err(error_info::SESSION_FAILED)
            .with_hint(format!("saved session not found: {selector}")));
    };
    match saved.kind {
        SavedSessionKind::Local => {
            SessionManager::start(&model.sessions, saved.name, saved.command)
        }
        SavedSessionKind::Remote => {
            let spec = RemoteSessionSpec::from_config(&model.config, saved.name, saved.command)?;
            SessionManager::start_remote(&model.sessions, spec)
        }
    }
}

fn start_and_remember_local(model: &mut TuiModel, name: String, command: String) -> Result<String> {
    let message = SessionManager::start(&model.sessions, name.clone(), command.clone())?;
    model.state.remember_session(SavedSession {
        name,
        kind: SavedSessionKind::Local,
        command,
    })?;
    Ok(message)
}

fn start_and_remember_remote(
    model: &mut TuiModel,
    name: String,
    command: String,
) -> Result<String> {
    let spec = RemoteSessionSpec::from_config(&model.config, name.clone(), command.clone())?;
    let message = SessionManager::start_remote(&model.sessions, spec)?;
    model.state.remember_session(SavedSession {
        name,
        kind: SavedSessionKind::Remote,
        command,
    })?;
    Ok(message)
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

fn focused_process_is_sync(model: &TuiModel) -> bool {
    model
        .processes
        .get(model.focused)
        .is_some_and(|process| process.session_id.is_none())
}

fn lock_sessions(
    model: &TuiModel,
) -> Result<std::sync::MutexGuard<'_, crate::session::SessionManager>> {
    model
        .sessions
        .lock()
        .map_err(|_| err(error_info::SESSION_FAILED).with_hint("session manager poisoned"))
}

fn push_result_event(model: &mut TuiModel, result: Result<String>) {
    match result {
        Ok(message) => model.push_event(message),
        Err(error) => model.push_event(tui_error_message(&error)),
    }
}

fn tui_error_message(error: &RdevError) -> String {
    match &error.hint {
        Some(hint) if !hint.is_empty() => format!("{error}; {hint}"),
        _ => error.to_string(),
    }
}

#[cfg(windows)]
fn write_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("powershell.exe")
        .arg("-NonInteractive")
        .arg("-NoProfile")
        .arg("-Command")
        .arg("[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(err(error_info::WATCH_EVENT_FAILED).with_hint("Set-Clipboard failed"))
    }
}

#[cfg(not(windows))]
fn write_clipboard(_text: &str) -> Result<()> {
    Err(err(error_info::WATCH_EVENT_FAILED).with_hint("clipboard copy is only wired on Windows"))
}

fn events_height(area: Rect) -> u16 {
    if area.height < 20 {
        0
    } else {
        1
    }
}
