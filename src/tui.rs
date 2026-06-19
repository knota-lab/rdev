use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, TryLockError};
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
use ratatui::widgets::{Block, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use notify::{RecursiveMode, Watcher};

use crate::cli::{DaemonArgs, DaemonCommand};
use crate::config::AppConfig;
use crate::daemon::{daemon_status_snapshot, run_daemon_command, DaemonStatusSnapshot};
use crate::error::{err, err_with_source, RdevError, Result};
use crate::error_info;
use crate::process::SystemProcessRunner;
use crate::session::{
    help_text, parse_console_command, ConsoleCommand, RemoteSessionSpec, SessionManager,
    SharedSessions,
};
use crate::sftp::SftpDeltaBackend;
use crate::sync::{RsyncSyncBackend, SyncDeltaRequest, SyncRequest};
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
use self::logs::LOG_PREFIX_WIDTH;
use self::logs::{parse_log_line, selected_line, wrapped_log_rows, RenderLogRow, UiLogLine};
use self::state::{SavedSession, SavedSessionKind, TuiStateStore};

const INPUT_PROMPT: &str = "rdev> ";
const PROCESS_PANEL_MIN_WIDTH: u16 = 30;
const PROCESS_PANEL_MAX_WIDTH: u16 = 48;
const EVENT_POLL: Duration = Duration::from_millis(100);
const DAEMON_STATUS_POLL: Duration = Duration::from_secs(1);
const COMMAND_HISTORY_LIMIT: usize = 100;
const BG_MAIN: Color = Color::Rgb(10, 10, 10);
const BG_RAIL: Color = Color::Rgb(22, 22, 22);
const BG_INPUT: Color = Color::Rgb(36, 36, 36);
const ORANGE: Color = Color::Rgb(255, 165, 0);

#[derive(Debug, Clone)]
pub struct TuiRequest {
    pub project_root: PathBuf,
    pub poll: bool,
}

#[derive(Debug)]
struct TuiModel {
    config: AppConfig,
    project_root: PathBuf,
    state: TuiStateStore,
    sessions: SharedSessions,
    project: String,
    remote: String,
    sync_status: ProcessStatus,
    daemon_status: DaemonStatusSnapshot,
    daemon_last_checked: Instant,
    processes: Vec<UiProcess>,
    focused: usize,
    logs: Vec<UiLogLine>,
    sync_logs: Vec<UiLogLine>,
    sync_log_version: u64,
    focused_log_session: Option<u32>,
    focused_log_version: u64,
    events: Vec<UiEvent>,
    input: String,
    input_cursor: usize,
    command_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    command_status: Option<CommandStatus>,
    follow_logs: bool,
    log_scroll: u16,
    log_max_scroll: u16,
    log_region: Option<LogRegion>,
    log_rows_cache: Option<LogRowsCache>,
    selection: Option<TextSelection>,
    help_visible: bool,
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

struct TuiCommandRuntime {
    sender: mpsc::Sender<CommandJob>,
    receiver: mpsc::Receiver<CommandWorkerEvent>,
    in_flight: bool,
}

struct TuiEventRuntime<'a> {
    sync: &'a mut TuiSyncRuntime,
    commands: &'a mut TuiCommandRuntime,
}

#[derive(Debug)]
struct CommandStatus {
    label: String,
    started: Instant,
}

enum CommandJob {
    StopSession { selector: String },
    StopDaemon,
    RestartDaemon,
}

impl CommandJob {
    fn label(&self) -> String {
        match self {
            Self::StopSession { selector } => format!("stop {selector}"),
            Self::StopDaemon => "daemon stop".to_owned(),
            Self::RestartDaemon => "daemon restart".to_owned(),
        }
    }
}

struct CommandWorkerEvent {
    label: String,
    result: std::result::Result<String, String>,
    refresh_daemon: bool,
}

struct SyncJob {
    project_root: PathBuf,
    kind: SyncJobKind,
    cancel: Arc<AtomicBool>,
}

enum SyncJobKind {
    Delta(PendingChanges),
    Full { delete: bool },
}

enum SyncWorkerEvent {
    Output(String),
    Done {
        kind: SyncJobDoneKind,
        cancelled: bool,
        result: std::result::Result<String, String>,
    },
}

enum SyncJobDoneKind {
    Delta(PendingChanges),
    Full,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusedLogSource {
    Sync,
    Daemon,
    Session { id: u32, version: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UiEvent {
    level: UiEventLevel,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiEventLevel {
    Info,
    Success,
    Warning,
    Error,
    Sync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventPanelMode {
    Compact,
    Rail,
}

impl UiEventLevel {
    fn parts(self) -> (&'static str, Style) {
        match self {
            Self::Info => ("info", Style::default().fg(Color::White)),
            Self::Success => ("ok", Style::default().fg(Color::Green)),
            Self::Warning => ("warn", Style::default().fg(Color::Yellow)),
            Self::Error => ("error", Style::default().fg(Color::Red)),
            Self::Sync => ("sync", Style::default().fg(Color::Cyan)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogRegion {
    content: Rect,
    first_row: usize,
    rows: Vec<LogRegionRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogRegionRow {
    text: String,
    starts_log_line: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogRowsCache {
    width: u16,
    rows: Vec<RenderLogRow>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventOutcome {
    quit: bool,
    dirty: bool,
    skip_refresh_once: bool,
}

impl EventOutcome {
    const CLEAN: Self = Self {
        quit: false,
        dirty: false,
        skip_refresh_once: false,
    };

    const DIRTY: Self = Self {
        quit: false,
        dirty: true,
        skip_refresh_once: false,
    };

    const QUIT: Self = Self {
        quit: true,
        dirty: false,
        skip_refresh_once: false,
    };

    const SCROLL: Self = Self {
        quit: false,
        dirty: true,
        skip_refresh_once: true,
    };
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
    let mut commands = TuiCommandRuntime::new(model.sessions.clone(), model.project_root.clone());
    let mut dirty = true;
    let mut skip_refresh_once = false;
    loop {
        dirty |= sync.process_events(config, &mut model);
        dirty |= commands.process_events(&mut model);
        if skip_refresh_once {
            skip_refresh_once = false;
        } else {
            dirty |= model.refresh_sessions();
        }
        if model.command_status.is_some() {
            dirty = true;
        }
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
            let mut runtime = TuiEventRuntime {
                sync: &mut sync,
                commands: &mut commands,
            };
            let outcome = handle_event(&mut model, &mut runtime, event);
            if outcome.quit {
                if let Ok(mut manager) = model.sessions.lock() {
                    manager.stop_all();
                }
                return Ok(());
            }
            dirty |= outcome.dirty;
            skip_refresh_once |= outcome.skip_refresh_once;
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
        let sessions = SessionManager::shared(request.project_root.clone());
        let mut model = Self {
            config: config.clone(),
            project_root: request.project_root.clone(),
            command_history: state.command_history().to_vec(),
            state,
            sessions,
            project,
            remote,
            sync_status: ProcessStatus::Idle,
            daemon_status: daemon_status_snapshot(&request.project_root),
            daemon_last_checked: Instant::now(),
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
            sync_log_version: 1,
            focused_log_session: None,
            focused_log_version: 0,
            events: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            history_index: None,
            history_draft: String::new(),
            command_status: None,
            follow_logs: true,
            log_scroll: 0,
            log_max_scroll: 0,
            log_region: None,
            log_rows_cache: None,
            selection: None,
            help_visible: false,
        };
        model.push_sync_event("sync watcher started");
        model.push_event("real sessions enabled");
        model.push_event("type new session <name> -- <command>");
        if let Some(event) = state_event {
            model.push_warning(event);
        }
        model
    }

    fn refresh_sessions(&mut self) -> bool {
        if self.input.is_empty() && self.daemon_last_checked.elapsed() >= DAEMON_STATUS_POLL {
            self.daemon_status = daemon_status_snapshot(&self.project_root);
            self.daemon_last_checked = Instant::now();
        }
        let previous_session = self
            .processes
            .get(self.focused)
            .and_then(|process| process.session_id);
        let previous_process_name = self
            .processes
            .get(self.focused)
            .map(|process| process.name.clone());
        let snapshot = {
            match self.sessions.try_lock() {
                Ok(mut manager) => Some(manager.snapshot()),
                Err(TryLockError::WouldBlock) => return false,
                Err(TryLockError::Poisoned(_)) => None,
            }
        };
        let Some(snapshot) = snapshot else {
            self.push_error("session manager unavailable");
            return true;
        };
        let mut processes = vec![self.sync_process(), self.daemon_process()];
        processes.extend(snapshot.sessions.iter().map(|session| UiProcess {
            id: session.id,
            session_id: Some(session.id),
            name: session.name.clone(),
            kind: session.kind.clone(),
            status: ProcessStatus::from_session(session.status.as_str(), session.exit_code),
            command: session.command.clone(),
        }));
        let focused = if previous_session.is_none() {
            previous_process_name
                .as_ref()
                .and_then(|name| {
                    processes
                        .iter()
                        .position(|process| process.name.as_str() == name.as_str())
                })
                .unwrap_or_else(|| self.focused.min(processes.len().saturating_sub(1)))
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
        let focused_session = processes
            .get(focused)
            .and_then(|process| process.session_id);
        let focused_log_version = focused_session
            .and_then(|id| {
                snapshot
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
                    .map(|session| session.log_version)
            })
            .unwrap_or(0);
        let focused_process_name = processes.get(focused).map(|process| process.name.clone());
        let focused_process_changed = self.focused != focused
            || previous_process_name.as_deref() != focused_process_name.as_deref()
            || previous_session != focused_session;
        let log_source = match focused_session {
            Some(id) => FocusedLogSource::Session {
                id,
                version: focused_log_version,
            },
            None if focused_process_name.as_deref() == Some("daemon") => FocusedLogSource::Daemon,
            None => FocusedLogSource::Sync,
        };
        let next_logs = self.next_logs(log_source);
        let logs_changed = next_logs.as_ref().is_some_and(|logs| self.logs != *logs);
        let dirty = focused_process_changed || self.processes != processes || logs_changed;
        self.focused = focused;
        self.processes = processes;
        if let Some(logs) = next_logs {
            self.logs = logs;
        }
        if focused_process_changed {
            self.selection = None;
            self.log_rows_cache = None;
        } else if logs_changed {
            self.log_rows_cache = None;
        }
        dirty
    }

    fn sync_process(&self) -> UiProcess {
        UiProcess {
            id: 0,
            session_id: None,
            name: "sync".to_owned(),
            kind: "watcher".to_owned(),
            status: self.sync_status,
            command: "file watcher".to_owned(),
        }
    }

    fn daemon_process(&self) -> UiProcess {
        let status = if self.daemon_status.running {
            if self.daemon_status.busy {
                ProcessStatus::Running
            } else {
                ProcessStatus::Idle
            }
        } else {
            ProcessStatus::Stopped
        };
        let command = if let Some(active_job) = &self.daemon_status.active_job {
            format!("persistent ssh daemon; active={active_job}")
        } else if let Some(addr) = &self.daemon_status.addr {
            format!("persistent ssh daemon; addr={addr}")
        } else {
            "persistent ssh daemon".to_owned()
        };
        UiProcess {
            id: self.daemon_status.pid.unwrap_or(0),
            session_id: None,
            name: "daemon".to_owned(),
            kind: "ssh".to_owned(),
            status,
            command,
        }
    }

    fn next_logs(&mut self, source: FocusedLogSource) -> Option<Vec<UiLogLine>> {
        match source {
            FocusedLogSource::Daemon => {
                self.focused_log_session = None;
                self.focused_log_version = 0;
                Some(self.daemon_logs())
            }
            FocusedLogSource::Sync => {
                if self.focused_log_session.is_none()
                    && self.focused_log_version == self.sync_log_version
                {
                    return None;
                }
                self.focused_log_session = None;
                self.focused_log_version = self.sync_log_version;
                Some(non_empty_logs(self.sync_logs.clone()))
            }
            FocusedLogSource::Session { id, version } => {
                if self.focused_log_session == Some(id) && self.focused_log_version == version {
                    return None;
                }
                let logs = lock_sessions(self)
                    .and_then(|mut manager| manager.logs_snapshot(id))
                    .map(|(version, logs)| {
                        self.focused_log_session = Some(id);
                        self.focused_log_version = version;
                        non_empty_logs(logs.into_iter().map(|line| parse_log_line(&line)).collect())
                    })
                    .unwrap_or_else(|error| vec![UiLogLine::rdev(tui_error_message(&error))]);
                Some(logs)
            }
        }
    }

    fn daemon_logs(&self) -> Vec<UiLogLine> {
        if !self.daemon_status.running {
            return vec![UiLogLine::rdev(
                "daemon is stopped; run daemon start to enable persistent rdev exec",
            )];
        }
        let mut lines = vec![UiLogLine::rdev("daemon is running")];
        if let Some(pid) = self.daemon_status.pid {
            lines.push(UiLogLine::rdev(format!("pid={pid}")));
        }
        if let Some(addr) = &self.daemon_status.addr {
            lines.push(UiLogLine::rdev(format!("addr={addr}")));
        }
        if let Some(remote) = &self.daemon_status.remote {
            lines.push(UiLogLine::rdev(format!("remote={remote}")));
        }
        lines.push(UiLogLine::rdev(format!("busy={}", self.daemon_status.busy)));
        lines.push(UiLogLine::rdev(format!(
            "active_job={}",
            self.daemon_status.active_job.as_deref().unwrap_or("<none>")
        )));
        lines
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
        self.log_rows_cache = None;
        self.push_event(format!("focused {}", self.processes[self.focused].name));
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
        self.log_rows_cache = None;
        self.push_event(format!("focused {}", self.processes[self.focused].name));
        self.focus_manager_to_current();
    }

    fn push_event(&mut self, event: impl Into<String>) {
        self.push_tui_event(UiEventLevel::Info, event);
    }

    fn push_success(&mut self, event: impl Into<String>) {
        self.push_tui_event(UiEventLevel::Success, event);
    }

    fn push_warning(&mut self, event: impl Into<String>) {
        self.push_tui_event(UiEventLevel::Warning, event);
    }

    fn push_error(&mut self, event: impl Into<String>) {
        self.push_tui_event(UiEventLevel::Error, event);
    }

    fn push_sync_event(&mut self, event: impl Into<String>) {
        self.push_tui_event(UiEventLevel::Sync, event);
    }

    fn push_tui_event(&mut self, level: UiEventLevel, event: impl Into<String>) {
        self.events.push(UiEvent {
            level,
            message: event.into(),
        });
        if self.events.len() > 8 {
            let overflow = self.events.len() - 8;
            self.events.drain(0..overflow);
        }
    }

    fn push_sync_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        self.sync_logs.push(UiLogLine::rdev(line.clone()));
        self.sync_log_version = self.sync_log_version.saturating_add(1);
        self.push_sync_event(line);
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
            Err(error) => self.push_error(error.to_string()),
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
                    kind,
                    cancelled,
                    result,
                } => {
                    self.worker.in_flight = false;
                    self.worker.cancel = None;
                    match result {
                        Ok(message) => {
                            match kind {
                                SyncJobDoneKind::Delta(changes) => {
                                    self.synced_files.record(&changes, &self.local_root);
                                }
                                SyncJobDoneKind::Full => {
                                    self.synced_files.record_existing(&EventFilter {
                                        local_root: &self.local_root,
                                        watch_dirs: &config.sync.watch_dirs,
                                        excludes: &config.sync.exclude,
                                    });
                                }
                            }
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
            kind: SyncJobKind::Delta(changes),
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

    fn start_full_sync(&mut self, config: &AppConfig, model: &mut TuiModel) {
        if self.worker.in_flight {
            model.push_warning("sync is already running");
            return;
        }
        model.sync_status = ProcessStatus::Running;
        model.push_sync_log("full sync requested");
        let cancel = Arc::new(AtomicBool::new(false));
        match self.worker.sender.send(SyncJob {
            project_root: self.local_root.clone(),
            kind: SyncJobKind::Full {
                delete: config.sync.delete,
            },
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
                    let (kind, result) = match job.kind {
                        SyncJobKind::Delta(changes) => {
                            let request = SyncDeltaRequest {
                                project_root: job.project_root,
                                uploads: changes.uploads.iter().cloned().collect(),
                                deletes: changes.deletes.iter().cloned().collect(),
                                cancelled: Some(Arc::clone(&job.cancel)),
                            };
                            (
                                SyncJobDoneKind::Delta(changes),
                                backend
                                    .sync_delta(request)
                                    .map(|report| report.format_text())
                                    .map_err(|error| error.to_string()),
                            )
                        }
                        SyncJobKind::Full { delete } => {
                            let request = SyncRequest {
                                dry_run: false,
                                delete,
                                project_root: job.project_root,
                                cancelled: Some(Arc::clone(&job.cancel)),
                            };
                            (
                                SyncJobDoneKind::Full,
                                backend
                                    .sync_full(request)
                                    .map(|report| report.format_text())
                                    .map_err(|error| error.to_string()),
                            )
                        }
                    };
                    let _send_result = event_sender.send(SyncWorkerEvent::Done {
                        kind,
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

impl TuiCommandRuntime {
    fn new(sessions: SharedSessions, project_root: PathBuf) -> Self {
        let (job_sender, job_receiver) = mpsc::channel::<CommandJob>();
        let (event_sender, event_receiver) = mpsc::channel::<CommandWorkerEvent>();
        thread::spawn(move || {
            while let Ok(job) = job_receiver.recv() {
                let (label, refresh_daemon, result) = match job {
                    CommandJob::StopSession { selector } => {
                        let result = sessions
                            .lock()
                            .map_err(|_| "session manager poisoned".to_owned())
                            .and_then(|mut manager| {
                                manager.stop(&selector).map_err(|error| error.to_string())
                            });
                        (format!("stop {selector}"), false, result)
                    }
                    CommandJob::StopDaemon => {
                        let result = run_daemon_command(
                            DaemonArgs {
                                command: DaemonCommand::Stop,
                            },
                            &project_root,
                        )
                        .map_err(|error| error.to_string());
                        ("daemon stop".to_owned(), true, result)
                    }
                    CommandJob::RestartDaemon => {
                        let _stop_result = run_daemon_command(
                            DaemonArgs {
                                command: DaemonCommand::Stop,
                            },
                            &project_root,
                        );
                        let result = run_daemon_command(
                            DaemonArgs {
                                command: DaemonCommand::Start,
                            },
                            &project_root,
                        )
                        .map_err(|error| error.to_string());
                        ("daemon restart".to_owned(), true, result)
                    }
                };
                let _send_result = event_sender.send(CommandWorkerEvent {
                    label,
                    result,
                    refresh_daemon,
                });
            }
        });
        Self {
            sender: job_sender,
            receiver: event_receiver,
            in_flight: false,
        }
    }

    fn process_events(&mut self, model: &mut TuiModel) -> bool {
        let mut dirty = false;
        while let Ok(event) = self.receiver.try_recv() {
            self.in_flight = false;
            model.command_status = None;
            if event.refresh_daemon {
                model.daemon_status = daemon_status_snapshot(&model.project_root);
                model.daemon_last_checked = Instant::now();
            }
            match event.result {
                Ok(message) => {
                    for line in message.lines().filter(|line| !line.is_empty()) {
                        model.push_success(line.to_owned());
                    }
                }
                Err(error) => model.push_error(format!("{} failed: {error}", event.label)),
            }
            model.refresh_sessions();
            dirty = true;
        }
        dirty
    }

    fn start(&mut self, model: &mut TuiModel, job: CommandJob) {
        let label = job.label();
        if self.in_flight {
            model.push_warning("another command is still running");
            return;
        }
        match self.sender.send(job) {
            Ok(()) => {
                self.in_flight = true;
                model.command_status = Some(CommandStatus {
                    label: label.clone(),
                    started: Instant::now(),
                });
                model.push_event(format!("{label} started"));
            }
            Err(error) => model.push_error(format!("command worker stopped: {error}")),
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
            Constraint::Length(3),
        ])
        .split(area);

    draw_status(frame, vertical[0], model);
    draw_body(frame, vertical[1], model);
    draw_input(frame, vertical[2], model);
    set_input_cursor(frame, vertical[2], model);
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

fn fill_area(frame: &mut Frame<'_>, area: Rect, color: Color) {
    frame.render_widget(Block::default().style(Style::default().bg(color)), area);
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, model: &mut TuiModel) {
    if area.width < 80 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(4),
                Constraint::Length(compact_events_height(area)),
            ])
            .split(area);
        draw_logs(frame, chunks[0], model);
        draw_compact_processes(frame, chunks[1], model);
        draw_events(frame, chunks[2], (model, EventPanelMode::Compact));
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
        || "Logs".to_owned(),
        |process| format!("Logs: {}", process.name),
    );
    fill_area(frame, area, BG_MAIN);
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ))),
            Rect {
                x: area.x.saturating_add(1),
                y: area.y,
                width: area.width.saturating_sub(2),
                height: 1,
            },
        );
    }

    let content = log_content_area(area);
    if model.help_visible {
        model.log_region = None;
        draw_help_panel(frame, content);
        return;
    }
    if model
        .log_rows_cache
        .as_ref()
        .map_or(true, |cache| cache.width != content.width)
    {
        model.log_rows_cache = Some(LogRowsCache {
            width: content.width,
            rows: wrapped_log_rows(&model.logs, content.width),
        });
    }
    let all_rows = model
        .log_rows_cache
        .as_ref()
        .map(|cache| cache.rows.as_slice())
        .unwrap_or(&[]);
    model.log_max_scroll = max_visual_log_scroll(all_rows.len(), content.height);
    if model.follow_logs {
        model.log_scroll = model.log_max_scroll;
    } else {
        model.log_scroll = model.log_scroll.min(model.log_max_scroll);
    }
    let scroll = model.log_scroll as usize;
    let visible_len = content.height as usize;
    let visible_rows = all_rows.iter().skip(scroll).take(visible_len);
    let mut region_rows = Vec::with_capacity(visible_len);
    for (index, row) in visible_rows.enumerate() {
        let absolute_row = scroll + index;
        region_rows.push(LogRegionRow {
            text: row.plain.clone(),
            starts_log_line: row.starts_log_line,
        });
        let line = selected_line(row, absolute_row, model.selection);
        frame.buffer_mut().set_line(
            content.x,
            content.y.saturating_add(index as u16),
            &line,
            content.width,
        );
    }
    model.log_region = Some(LogRegion {
        content,
        first_row: scroll,
        rows: region_rows,
    });
}

fn draw_help_panel(frame: &mut Frame<'_>, area: Rect) {
    let lines = help_text()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            if line.trim_end() == "Commands" || line.trim_end() == "Keys" {
                Line::from(Span::styled(
                    line.trim_end().to_owned(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(
                    line.trim_end().to_owned(),
                    Style::default().fg(Color::DarkGray),
                ))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn log_content_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn max_visual_log_scroll(row_count: usize, visible_rows: u16) -> u16 {
    if visible_rows == 0 {
        return 0;
    }
    (row_count as u16).saturating_sub(visible_rows)
}

fn draw_process_panel(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    fill_area(frame, area, BG_RAIL);
    let content = Rect {
        x: area.x.saturating_add(2),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(3),
        height: area.height.saturating_sub(1),
    };
    if content.height < 16 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(5)])
            .split(content);
        draw_side_context(frame, chunks[0], model);
        draw_process_list(frame, chunks[1], model);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Percentage(34),
            Constraint::Min(7),
            Constraint::Length(7),
        ])
        .split(content);
    draw_side_context(frame, chunks[0], model);
    draw_process_list(frame, chunks[1], model);
    draw_events(frame, chunks[2], (model, EventPanelMode::Rail));
    draw_process_details(frame, chunks[3], model);
}

fn draw_side_context(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    fill_area(frame, area, BG_RAIL);
    let focused = model
        .focused_process()
        .map_or("<none>", |process| process.name.as_str());
    let lines = vec![
        Line::from(Span::styled(
            model.project.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Context",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled("remote ", Style::default().fg(Color::DarkGray)),
            Span::raw(model.remote.clone()),
        ]),
        Line::from(vec![
            Span::styled("sync   ", Style::default().fg(Color::DarkGray)),
            Span::styled(model.sync_status.label(), model.sync_status.style()),
        ]),
        Line::from(vec![
            Span::styled("focus  ", Style::default().fg(Color::DarkGray)),
            Span::raw(focused.to_owned()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_process_list(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    fill_area(frame, area, BG_RAIL);
    let mut items = vec![ListItem::new(Line::from(Span::styled(
        "Processes",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )))];
    items.extend(model.processes.iter().enumerate().map(|(index, process)| {
        let marker = if index == model.focused { ">" } else { " " };
        let shortcut = process_shortcut_label(index);
        let style = if index == model.focused {
            process.status.style().add_modifier(Modifier::BOLD)
        } else {
            process.status.style()
        };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(Color::Cyan)),
            Span::styled(format!("{shortcut} "), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:<12}", process.name), style),
            Span::styled(process.status.label(), process.status.style()),
        ]))
    }));
    let list = List::new(items);
    frame.render_widget(list, area);
}

fn draw_process_details(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    fill_area(frame, area, BG_RAIL);
    let lines = model.focused_process().map_or_else(
        || {
            vec![
                Line::from(Span::styled(
                    "Details",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("no process"),
            ]
        },
        |process| {
            vec![
                Line::from(Span::styled(
                    "Details",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(vec![
                    Span::styled("id     ", Style::default().fg(Color::DarkGray)),
                    Span::raw(process.id.to_string()),
                ]),
                Line::from(vec![
                    Span::styled("kind   ", Style::default().fg(Color::DarkGray)),
                    Span::raw(process.kind.clone()),
                ]),
                Line::from(vec![
                    Span::styled("status ", Style::default().fg(Color::DarkGray)),
                    Span::styled(process.status.label(), process.status.style()),
                ]),
                Line::from(Span::styled("cmd", Style::default().fg(Color::DarkGray))),
                Line::from(process.command.as_str()),
            ]
        },
    );
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_compact_processes(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    fill_area(frame, area, BG_RAIL);
    let spans = model
        .processes
        .iter()
        .enumerate()
        .flat_map(|(index, process)| {
            let marker = if index == model.focused { "> " } else { "" };
            let shortcut = process_shortcut_label(index);
            [
                Span::styled(
                    format!(
                        "{marker}{shortcut} {}:{} ",
                        process.name,
                        process.status.label()
                    ),
                    process.status.style(),
                ),
                Span::raw(" "),
            ]
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn process_shortcut_label(index: usize) -> String {
    if index < 9 {
        (index + 1).to_string()
    } else {
        "-".to_owned()
    }
}

fn draw_events(frame: &mut Frame<'_>, area: Rect, options: (&TuiModel, EventPanelMode)) {
    let (model, mode) = options;
    if area.height == 0 {
        return;
    }
    fill_area(frame, area, BG_RAIL);
    let content = area;
    if content.height == 0 || content.width == 0 {
        return;
    }
    let Some(latest) = model.events.last() else {
        frame.render_widget(Paragraph::new(Line::from("status: idle")), content);
        return;
    };
    let mut lines = if mode == EventPanelMode::Rail {
        vec![
            Line::from(Span::styled(
                "Events",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            event_line("latest", latest),
        ]
    } else {
        vec![event_line("latest", latest)]
    };
    if content.height > 2 {
        let history = model
            .events
            .iter()
            .rev()
            .skip(1)
            .take(content.height.saturating_sub(1) as usize)
            .map(event_history_line)
            .collect::<Vec<_>>();
        if !history.is_empty() {
            lines.push(Line::from(Span::styled(
                "recent",
                Style::default().fg(Color::DarkGray),
            )));
            lines.extend(history);
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), content);
}

fn event_line(title: &str, event: &UiEvent) -> Line<'static> {
    let (label, style) = event.level.parts();
    Line::from(vec![
        Span::styled(format!("{title} "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{label} "), style.add_modifier(Modifier::BOLD)),
        Span::styled(event.message.clone(), style),
    ])
}

fn event_history_line(event: &UiEvent) -> Line<'static> {
    let (label, style) = event.level.parts();
    Line::from(vec![
        Span::styled(format!("{label} "), style),
        Span::styled(event.message.clone(), Style::default().fg(Color::DarkGray)),
    ])
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let input_area = input_text_area(area);
    frame.render_widget(Block::default().style(Style::default().bg(BG_INPUT)), area);
    let (prompt, prompt_style) = input_prompt(model);
    let mut spans = vec![
        Span::styled(prompt, prompt_style.add_modifier(Modifier::BOLD)),
        Span::styled(model.input.as_str(), Style::default().fg(Color::White)),
    ];
    if model.input.is_empty() {
        if let Some(status) = &model.command_status {
            spans.push(Span::styled(
                format!("{}...", status.label),
                Style::default().fg(ORANGE),
            ));
        }
    }
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), input_area);
}

fn set_input_cursor(frame: &mut Frame<'_>, area: Rect, model: &TuiModel) {
    let input_area = input_text_area(area);
    if input_area.height == 0 || input_area.width == 0 {
        return;
    }
    let (prompt, _) = input_prompt(model);
    let prompt_width = UnicodeWidthStr::width(prompt.as_str()) as u16;
    let input_width = UnicodeWidthStr::width(&model.input[..model.input_cursor]) as u16;
    let max_x = input_area.width.saturating_sub(1);
    let cursor_x = prompt_width.saturating_add(input_width).min(max_x);
    frame.set_cursor(input_area.x.saturating_add(cursor_x), input_area.y);
}

fn input_prompt(model: &TuiModel) -> (String, Style) {
    let Some(status) = &model.command_status else {
        return (INPUT_PROMPT.to_owned(), Style::default().fg(Color::Cyan));
    };
    let frames = ['|', '/', '-', '\\'];
    let index = (status.started.elapsed().as_millis() / 120) as usize % frames.len();
    (
        format!("rdev{} ", frames[index]),
        Style::default().fg(ORANGE),
    )
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

fn handle_event(
    model: &mut TuiModel,
    runtime: &mut TuiEventRuntime<'_>,
    event: Event,
) -> EventOutcome {
    match event {
        Event::Key(key) => handle_key(model, runtime, key),
        Event::Paste(text) => {
            model.insert_text(&text);
            EventOutcome::DIRTY
        }
        Event::Mouse(mouse) => {
            let dirty = match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    let Some(pos) = selection_pos(model, mouse.column, mouse.row) else {
                        return EventOutcome::CLEAN;
                    };
                    let next = if extends_selection(mouse.modifiers) {
                        TextSelection {
                            anchor: model.selection.map_or(pos, |selection| selection.anchor),
                            cursor: pos,
                        }
                    } else {
                        TextSelection {
                            anchor: pos,
                            cursor: pos,
                        }
                    };
                    set_selection_if_changed(model, Some(next))
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(pos) = selection_pos(model, mouse.column, mouse.row) {
                        if let Some(selection) = model.selection.as_mut() {
                            if selection.cursor != pos {
                                selection.cursor = pos;
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => false,
                MouseEventKind::Down(MouseButton::Right) => copy_selection(model),
                MouseEventKind::ScrollUp => {
                    model.scroll_logs_up(3);
                    model.selection = None;
                    return EventOutcome::SCROLL;
                }
                MouseEventKind::ScrollDown => {
                    model.scroll_logs_down(3);
                    model.selection = None;
                    return EventOutcome::SCROLL;
                }
                _ => false,
            };
            EventOutcome {
                quit: false,
                dirty,
                skip_refresh_once: false,
            }
        }
        Event::Resize(_, _) => EventOutcome::DIRTY,
        _ => EventOutcome::CLEAN,
    }
}

fn handle_key(
    model: &mut TuiModel,
    runtime: &mut TuiEventRuntime<'_>,
    key: KeyEvent,
) -> EventOutcome {
    if key.kind != KeyEventKind::Press {
        return EventOutcome::CLEAN;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if copy_selection(model) {
            return EventOutcome::DIRTY;
        }
        if focused_process_is_sync(model) && runtime.sync.cancel_current(model) {
            return EventOutcome::DIRTY;
        }
        if focused_process_is_sync(model) {
            model.sync_status = ProcessStatus::Cancelled;
            model.push_warning("sync cancel requested");
        } else {
            model.push_warning("ctrl+c copies selection; focus sync to cancel sync");
        }
        return EventOutcome::DIRTY;
    }
    match key.code {
        KeyCode::Char('?') => {
            model.help_visible = true;
            model.selection = None;
            model.push_event("help panel opened; press Esc to close");
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
        KeyCode::Enter => {
            return if submit_input(model, runtime) {
                EventOutcome::QUIT
            } else {
                EventOutcome::DIRTY
            }
        }
        KeyCode::Esc => {
            model.help_visible = false;
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
            model.scroll_logs_up(10);
            return EventOutcome::SCROLL;
        }
        KeyCode::PageDown if model.input.is_empty() => {
            model.scroll_logs_down(10);
            return EventOutcome::SCROLL;
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
    EventOutcome::DIRTY
}

impl TuiModel {
    fn scroll_logs_up(&mut self, amount: u16) {
        if self.follow_logs {
            self.log_scroll = self.log_max_scroll;
        }
        self.follow_logs = false;
        self.log_scroll = self.log_scroll.saturating_sub(amount);
    }

    fn scroll_logs_down(&mut self, amount: u16) {
        self.log_scroll = self
            .log_scroll
            .saturating_add(amount)
            .min(self.log_max_scroll);
        self.follow_logs = self.log_scroll >= self.log_max_scroll;
    }
}

fn selection_pos(model: &TuiModel, x: u16, y: u16) -> Option<CellPos> {
    let region = model.log_region.as_ref()?;
    if !contains(region.content, x, y) || region.rows.is_empty() {
        return None;
    }
    let visible_row = y.saturating_sub(region.content.y) as usize;
    let row = region.first_row.saturating_add(visible_row);
    let row = region.rows.get(visible_row).map(|line| CellPos {
        row,
        col: x
            .saturating_sub(region.content.x)
            .saturating_sub(LOG_PREFIX_WIDTH)
            .min(UnicodeWidthStr::width(line.text.as_str()) as u16),
    })?;
    Some(row)
}

fn non_empty_logs(mut logs: Vec<UiLogLine>) -> Vec<UiLogLine> {
    if logs.is_empty() {
        logs.push(UiLogLine::rdev("logs: <empty>"));
    }
    logs
}

fn extends_selection(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL)
}

fn set_selection_if_changed(model: &mut TuiModel, selection: Option<TextSelection>) -> bool {
    if model.selection == selection {
        return false;
    }
    model.selection = selection;
    true
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
            model.push_success(format!("copied {} lines", text.lines().count()));
            model.selection = None;
        }
        Err(error) => model.push_error(tui_error_message(&error)),
    }
    true
}

fn selected_text(model: &TuiModel) -> Option<String> {
    let selection = model.selection?;
    let region = model.log_region.as_ref()?;
    let (start, end) = ordered_selection(selection);
    let mut selected = String::new();
    for row in start.row..=end.row {
        let visible_row = row.checked_sub(region.first_row)?;
        let line = region.rows.get(visible_row)?;
        let text = line.text.as_str();
        let width = UnicodeWidthStr::width(text) as u16;
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
        if !selected.is_empty() && line.starts_log_line {
            selected.push('\n');
        }
        let start_index = byte_index_for_display_col(text, start_col);
        let end_index = byte_index_for_display_col(text, end_col);
        selected.push_str(&text[start_index..end_index]);
    }
    Some(selected)
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

fn submit_input(model: &mut TuiModel, runtime: &mut TuiEventRuntime<'_>) -> bool {
    let command = model.input.trim().to_owned();
    model.clear_input();
    if command.is_empty() {
        return false;
    }
    model.push_command_history(&command);
    execute_console_command(model, runtime, parse_console_command(&command))
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
    model.push_event(format!("focused {}", model.processes[model.focused].name));
    model.focus_manager_to_current();
}

fn execute_console_command(
    model: &mut TuiModel,
    runtime: &mut TuiEventRuntime<'_>,
    command: ConsoleCommand,
) -> bool {
    let result = match command {
        ConsoleCommand::Help => {
            model.help_visible = true;
            model.selection = None;
            model.push_event("help panel opened; press Esc to close");
            Ok(String::new())
        }
        ConsoleCommand::Sessions => lock_sessions(model).map(|mut manager| manager.list()),
        ConsoleCommand::NewSession { name, command } => {
            start_and_remember_local(model, name, command)
        }
        ConsoleCommand::NewRemoteSession { name, command } => {
            start_and_remember_remote(model, name, command)
        }
        ConsoleCommand::SavedSessions => Ok(model.state.saved_sessions_text()),
        ConsoleCommand::RestoreSession { selector } => restore_saved_session(model, &selector),
        ConsoleCommand::DeleteSavedSession { selector } => delete_saved_session(model, &selector),
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
            runtime
                .commands
                .start(model, CommandJob::StopSession { selector });
            Ok(String::new())
        }
        ConsoleCommand::StopFocused => {
            stop_focused(model, runtime.commands);
            Ok(String::new())
        }
        ConsoleCommand::Restart { selector } => SessionManager::restart(&model.sessions, &selector),
        ConsoleCommand::RestartFocused => {
            restart_focused(model, runtime.commands);
            Ok(String::new())
        }
        ConsoleCommand::Sync => {
            runtime.sync.start_full_sync(&model.config.clone(), model);
            Ok(String::new())
        }
        ConsoleCommand::DaemonStart => run_tui_daemon_command(model, DaemonCommand::Start),
        ConsoleCommand::DaemonStatus => run_tui_daemon_command(model, DaemonCommand::Status),
        ConsoleCommand::DaemonStop => {
            runtime.commands.start(model, CommandJob::StopDaemon);
            Ok(String::new())
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
        ConsoleCommand::Unknown(message) => {
            model.push_warning(format!("unknown command: {message}"));
            Ok(String::new())
        }
    };
    match result {
        Ok(message) => {
            for line in message.lines().filter(|line| !line.is_empty()) {
                model.push_success(line.to_owned());
            }
        }
        Err(error) => model.push_error(tui_error_message(&error)),
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
            SessionManager::restore(&model.sessions, saved.name, saved.command)
        }
        SavedSessionKind::Remote => {
            let spec = RemoteSessionSpec::from_config(&model.config, saved.name, saved.command)?;
            SessionManager::restore_remote(&model.sessions, spec)
        }
    }
}

fn delete_saved_session(model: &mut TuiModel, selector: &str) -> Result<String> {
    if let Some(saved) = model.state.find_session(selector) {
        let name = saved.name;
        let mut messages = vec![model.state.delete_session(selector)?];
        match lock_sessions(model).and_then(|mut manager| manager.delete_inactive(&name)) {
            Ok(message) => messages.push(message),
            Err(error) if is_session_not_found(&error) => {}
            Err(error) => messages.push(tui_error_message(&error)),
        }
        return Ok(messages.join("\n"));
    }

    match lock_sessions(model).and_then(|mut manager| manager.delete_inactive(selector)) {
        Ok(message) => Ok(message),
        Err(error) if is_session_not_found(&error) => model.state.delete_session(selector),
        Err(error) => Err(error),
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

fn run_tui_daemon_command(model: &mut TuiModel, command: DaemonCommand) -> Result<String> {
    let result = run_daemon_command(DaemonArgs { command }, &model.project_root);
    model.daemon_status = daemon_status_snapshot(&model.project_root);
    model.daemon_last_checked = Instant::now();
    result
}

fn stop_focused(model: &mut TuiModel, commands: &mut TuiCommandRuntime) {
    if focused_process_is_daemon(model) {
        commands.start(model, CommandJob::StopDaemon);
        return;
    }
    let Some(selector) = focused_session_selector(model) else {
        model.push_warning("sync watcher cannot be stopped in TUI yet");
        return;
    };
    commands.start(model, CommandJob::StopSession { selector });
}

fn restart_focused(model: &mut TuiModel, commands: &mut TuiCommandRuntime) {
    if focused_process_is_daemon(model) {
        commands.start(model, CommandJob::RestartDaemon);
        return;
    }
    let Some(selector) = focused_session_selector(model) else {
        model.push_warning("sync watcher cannot be restarted in TUI yet");
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
        .is_some_and(process_is_sync)
}

fn focused_process_is_daemon(model: &TuiModel) -> bool {
    model
        .processes
        .get(model.focused)
        .is_some_and(process_is_daemon)
}

fn process_is_sync(process: &UiProcess) -> bool {
    process.session_id.is_none() && process.name == "sync" && process.kind == "watcher"
}

fn process_is_daemon(process: &UiProcess) -> bool {
    process.session_id.is_none() && process.name == "daemon" && process.kind == "ssh"
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
        Ok(message) => model.push_success(message),
        Err(error) => model.push_error(tui_error_message(&error)),
    }
}

fn tui_error_message(error: &RdevError) -> String {
    match &error.hint {
        Some(hint) if !hint.is_empty() => format!("{error}; {hint}"),
        _ => error.to_string(),
    }
}

fn is_session_not_found(error: &RdevError) -> bool {
    error.hint.as_deref() == Some("session not found")
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

fn compact_events_height(area: Rect) -> u16 {
    if area.height < 22 {
        0
    } else {
        3
    }
}
