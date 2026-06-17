use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::ssh::shell_quote;

const MAX_LOG_LINES: usize = 500;
const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_SESSION_SHELL: &str = "bash";
const REMOTE_SESSION_ENV: &str = "for f in /etc/profile ~/.bash_profile ~/.bash_login ~/.profile ~/.bashrc; do [ -r \"$f\" ] && . \"$f\" >/dev/null 2>&1 || true; done; export NVM_DIR=\"${NVM_DIR:-$HOME/.nvm}\"; [ -s \"$NVM_DIR/nvm.sh\" ] && . \"$NVM_DIR/nvm.sh\" >/dev/null 2>&1 || true; export VOLTA_HOME=\"${VOLTA_HOME:-$HOME/.volta}\"; [ -d \"$VOLTA_HOME/bin\" ] && case \":$PATH:\" in *\":$VOLTA_HOME/bin:\"*) ;; *) export PATH=\"$VOLTA_HOME/bin:$PATH\" ;; esac; if command -v fnm >/dev/null 2>&1; then eval \"$(fnm env --shell bash)\" >/dev/null 2>&1 || true; fi; export PNPM_HOME=\"${PNPM_HOME:-$HOME/.local/share/pnpm}\"; case \":$PATH:\" in *\":$PNPM_HOME:\"*) ;; *) export PATH=\"$PNPM_HOME:$PATH\" ;; esac";
const REMOTE_SESSION_RUNNER: &str = "echo \"$$\" > \"$RDEV_SESSION_PID_FILE\"; trap 'rm -f \"$RDEV_SESSION_PID_FILE\"' EXIT; eval \"$RDEV_SESSION_COMMAND\"";

pub type SharedSessions = Arc<Mutex<SessionManager>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleCommand {
    Help,
    Sessions,
    NewSession {
        name: String,
        command: String,
    },
    NewRemoteSession {
        name: String,
        command: String,
    },
    SavedSessions,
    RestoreSession {
        selector: String,
    },
    DeleteSavedSession {
        selector: String,
    },
    Logs {
        selector: Option<String>,
    },
    Tail {
        selector: Option<String>,
        lines: usize,
    },
    ClearLogs {
        selector: Option<String>,
    },
    Focus {
        selector: String,
    },
    Stop {
        selector: String,
    },
    StopFocused,
    Restart {
        selector: String,
    },
    RestartFocused,
    Sync,
    Quit,
    QuitForce,
    Empty,
    Unknown(String),
}

#[derive(Debug)]
pub struct SessionManager {
    next_id: u32,
    sessions: BTreeMap<u32, ManagedSession>,
    names: BTreeMap<String, u32>,
    focused: Option<u32>,
    cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub focused: Option<u32>,
    pub sessions: Vec<SessionProcessSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProcessSnapshot {
    pub id: u32,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub logs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSessionSpec {
    name: String,
    command: String,
    host: String,
    port: u16,
    remote_root: String,
}

impl RemoteSessionSpec {
    pub fn from_config(config: &AppConfig, name: String, command: String) -> Result<Self> {
        let remote_root = RemotePath::parse(config.remote.path.as_str())?;
        Ok(Self {
            name,
            command,
            host: config.remote.host.clone(),
            port: config.remote.port,
            remote_root: remote_root.as_str().to_owned(),
        })
    }
}

#[derive(Debug)]
struct ManagedSession {
    id: u32,
    name: String,
    kind: SessionKind,
    launch: SessionLaunch,
    remote_control: Option<RemoteSessionControl>,
    command: String,
    status: SessionStatus,
    child: Option<Child>,
    logs: VecDeque<String>,
    exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Running,
    Exited,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKind {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionLaunch {
    Local,
    Remote(RemoteSessionSpec),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteSessionControl {
    host: String,
    port: u16,
    pid_file: String,
}

impl SessionManager {
    pub fn shared(cwd: PathBuf) -> SharedSessions {
        Arc::new(Mutex::new(Self {
            next_id: 1,
            sessions: BTreeMap::new(),
            names: BTreeMap::new(),
            focused: None,
            cwd,
        }))
    }

    pub fn start(shared: &SharedSessions, name: String, command: String) -> Result<String> {
        Self::start_with_command(
            shared,
            StartSpec::local(name, command).replace_inactive(),
            shell_command,
        )
    }

    pub fn restore(shared: &SharedSessions, name: String, command: String) -> Result<String> {
        Self::start_with_command(
            shared,
            StartSpec::local(name, command).replace_inactive(),
            shell_command,
        )
    }

    pub fn start_remote(shared: &SharedSessions, spec: RemoteSessionSpec) -> Result<String> {
        let remote = RemoteSession {
            host: spec.host.clone(),
            port: spec.port,
            remote_root: spec.remote_root.clone(),
        };
        let session_name = spec.name.clone();
        Self::start_with_command(
            shared,
            StartSpec::remote(spec).replace_inactive(),
            |command| remote.shell_command(&session_name, command),
        )
    }

    pub fn restore_remote(shared: &SharedSessions, spec: RemoteSessionSpec) -> Result<String> {
        let remote = RemoteSession {
            host: spec.host.clone(),
            port: spec.port,
            remote_root: spec.remote_root.clone(),
        };
        let session_name = spec.name.clone();
        Self::start_with_command(
            shared,
            StartSpec::remote(spec).replace_inactive(),
            |command| remote.shell_command(&session_name, command),
        )
    }

    fn start_with_command(
        shared: &SharedSessions,
        spec: StartSpec,
        build: impl FnOnce(&str) -> Command,
    ) -> Result<String> {
        let StartSpec {
            name,
            command,
            kind,
            launch,
            remote_control,
            conflict,
        } = spec;
        validate_session_name(&name)?;
        let cwd = {
            let mut manager = lock_sessions(shared)?;
            manager.refresh();
            if let Some(existing_id) = manager.names.get(&name).copied() {
                match conflict {
                    StartConflict::RejectExisting => {
                        return Err(err(error_info::SESSION_FAILED)
                            .with_hint(format!("已存在同名会话: {name}")));
                    }
                    StartConflict::ReplaceInactive => {
                        let Some(existing) = manager.sessions.get(&existing_id) else {
                            return Err(
                                err(error_info::SESSION_FAILED).with_hint("session not found")
                            );
                        };
                        if existing.status == SessionStatus::Running {
                            return Err(err(error_info::SESSION_FAILED)
                                .with_hint(format!("已存在正在运行的同名会话: {name}")));
                        }
                        manager.remove_session(&name)?;
                    }
                }
            }
            manager.cwd.clone()
        };
        let mut process = build(&command);
        let process_display = command_display(&process);
        let mut child = process
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = {
            let mut manager = lock_sessions(shared)?;
            let id = manager.next_id;
            manager.next_id += 1;
            manager.names.insert(name.clone(), id);
            manager.focused = Some(id);
            manager.sessions.insert(
                id,
                ManagedSession {
                    id,
                    name: name.clone(),
                    kind,
                    launch,
                    remote_control,
                    command: command.clone(),
                    status: SessionStatus::Running,
                    child: Some(child),
                    logs: VecDeque::from([format!("[rdev] spawn {process_display}")]),
                    exit_code: None,
                },
            );
            id
        };
        spawn_log_reader(LogReader {
            shared: Arc::clone(shared),
            id,
            stream: "stdout",
            reader: stdout,
        });
        spawn_log_reader(LogReader {
            shared: Arc::clone(shared),
            id,
            stream: "stderr",
            reader: stderr,
        });
        thread::sleep(Duration::from_millis(200));
        let mut manager = lock_sessions(shared)?;
        manager.refresh();
        let Some(session) = manager.sessions.get(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        if session.status != SessionStatus::Running {
            return Ok(format!(
                "session exited id={id} name={name} kind={} exit={} command={command}; run logs {name}",
                session.kind.label(),
                session
                    .exit_code
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            ));
        }
        Ok(format!(
            "session started id={id} name={name} kind={} command={command}",
            kind.label()
        ))
    }

    pub fn list(&mut self) -> String {
        self.refresh();
        if self.sessions.is_empty() {
            return "sessions: <none>".to_owned();
        }
        let mut lines = vec!["sessions:".to_owned()];
        for session in self.sessions.values() {
            let focus = if Some(session.id) == self.focused {
                "*"
            } else {
                " "
            };
            let exit = session
                .exit_code
                .map_or_else(|| "-".to_owned(), |code| code.to_string());
            lines.push(format!(
                "{focus} {} {} kind={} status={} exit={} command={}",
                session.id,
                session.name,
                session.kind.label(),
                session.status.label(),
                exit,
                session.command
            ));
        }
        lines.join("\n")
    }

    pub fn snapshot(&mut self) -> SessionSnapshot {
        self.refresh();
        SessionSnapshot {
            focused: self.focused,
            sessions: self
                .sessions
                .values()
                .map(|session| SessionProcessSnapshot {
                    id: session.id,
                    name: session.name.clone(),
                    kind: session.kind.label().to_owned(),
                    status: session.status.label().to_owned(),
                    command: session.command.clone(),
                    exit_code: session.exit_code,
                    logs: session.logs.iter().cloned().collect(),
                })
                .collect(),
        }
    }

    pub fn logs(&mut self, selector: Option<&str>) -> Result<String> {
        self.refresh();
        let id = self.resolve_or_focused(selector)?;
        let Some(session) = self.sessions.get(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        if session.logs.is_empty() {
            return Ok(format!("logs {}: <empty>", session.name));
        }
        Ok(session.logs.iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    pub fn tail_logs(&mut self, selector: Option<&str>, lines: usize) -> Result<String> {
        self.refresh();
        let id = self.resolve_or_focused(selector)?;
        let Some(session) = self.sessions.get(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        if session.logs.is_empty() {
            return Ok(format!("logs {}: <empty>", session.name));
        }
        let lines = lines.max(1);
        let skip = session.logs.len().saturating_sub(lines);
        Ok(session
            .logs
            .iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub fn clear_logs(&mut self, selector: Option<&str>) -> Result<String> {
        self.refresh();
        let id = self.resolve_or_focused(selector)?;
        let Some(session) = self.sessions.get_mut(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        session.logs.clear();
        Ok(format!("logs cleared: {}", session.name))
    }

    pub fn focus(&mut self, selector: &str) -> Result<String> {
        self.refresh();
        let id = self.resolve(selector)?;
        self.focused = Some(id);
        let Some(session) = self.sessions.get(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        Ok(format!("focused {} ({})", session.name, session.id))
    }

    pub fn stop(&mut self, selector: &str) -> Result<String> {
        self.refresh();
        let id = self.resolve(selector)?;
        self.stop_id(id)
    }

    pub fn delete_inactive(&mut self, selector: &str) -> Result<String> {
        self.refresh();
        let id = self.resolve(selector)?;
        let Some(session) = self.sessions.get(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        if session.status == SessionStatus::Running {
            return Err(err(error_info::SESSION_FAILED)
                .with_hint(format!("会话正在运行，不能删除: {}", session.name)));
        }
        let name = session.name.clone();
        self.remove_session(&name)?;
        Ok(format!("session deleted: {name}"))
    }

    pub fn restart(shared: &SharedSessions, selector: &str) -> Result<String> {
        let (name, command, launch) = {
            let mut manager = lock_sessions(shared)?;
            manager.refresh();
            let id = manager.resolve(selector)?;
            let Some(session) = manager.sessions.get(&id) else {
                return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
            };
            (
                session.name.clone(),
                session.command.clone(),
                session.launch.clone(),
            )
        };
        {
            let mut manager = lock_sessions(shared)?;
            let _stopped = manager.stop(&name)?;
            manager.remove_session(&name)?;
        }
        match launch {
            SessionLaunch::Local => Self::start(shared, name, command),
            SessionLaunch::Remote(mut spec) => {
                spec.name = name;
                spec.command = command;
                Self::start_remote(shared, spec)
            }
        }
    }

    pub fn has_running(&mut self) -> bool {
        self.refresh();
        self.sessions
            .values()
            .any(|session| session.status == SessionStatus::Running)
    }

    pub fn running_summary(&mut self) -> String {
        self.refresh();
        let running = self
            .sessions
            .values()
            .filter(|session| session.status == SessionStatus::Running)
            .map(|session| format!("{}({})", session.name, session.id))
            .collect::<Vec<_>>();
        if running.is_empty() {
            "running sessions: <none>".to_owned()
        } else {
            format!("running sessions: {}", running.join(", "))
        }
    }

    pub fn stop_all(&mut self) {
        let ids = self.sessions.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let _ignored = self.stop_id(id);
        }
    }

    fn stop_id(&mut self, id: u32) -> Result<String> {
        let Some(session) = self.sessions.get_mut(&id) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        if session.status != SessionStatus::Running {
            return Ok(format!(
                "session already {}: {}",
                session.status.label(),
                session.name
            ));
        }
        if let Some(control) = &session.remote_control {
            if let Err(error) = terminate_remote_session(control) {
                push_log(session, format!("[rdev] remote stop failed: {error}"));
            }
        }
        if let Some(mut child) = session.child.take() {
            terminate_child(&mut child)?;
            session.exit_code = wait_child_exit(&mut child)?;
        }
        session.status = SessionStatus::Stopped;
        push_log(session, "[rdev] stopped".to_owned());
        Ok(format!("session stopped: {}", session.name))
    }

    fn remove_session(&mut self, name: &str) -> Result<()> {
        let Some(id) = self.names.remove(name) else {
            return Err(err(error_info::SESSION_FAILED).with_hint("session not found"));
        };
        self.sessions.remove(&id);
        if self.focused == Some(id) {
            self.focused = None;
        }
        Ok(())
    }

    fn resolve_or_focused(&self, selector: Option<&str>) -> Result<u32> {
        match selector {
            Some(selector) => self.resolve(selector),
            None => self
                .focused
                .ok_or_else(|| err(error_info::SESSION_FAILED).with_hint("no focused session")),
        }
    }

    fn resolve(&self, selector: &str) -> Result<u32> {
        if let Ok(id) = selector.parse::<u32>() {
            if self.sessions.contains_key(&id) {
                return Ok(id);
            }
        }
        self.names
            .get(selector)
            .copied()
            .ok_or_else(|| err(error_info::SESSION_FAILED).with_hint("session not found"))
    }

    fn refresh(&mut self) {
        for session in self.sessions.values_mut() {
            if session.status != SessionStatus::Running {
                continue;
            }
            let Some(child) = session.child.as_mut() else {
                continue;
            };
            if let Ok(Some(status)) = child.try_wait() {
                session.exit_code = status.code();
                session.status = SessionStatus::Exited;
                session.child = None;
                push_log(
                    session,
                    format!("[rdev] exited code={}", status_code_label(status.code())),
                );
            }
        }
    }

    fn push_log(&mut self, id: u32, line: String) {
        if let Some(session) = self.sessions.get_mut(&id) {
            push_log(session, line);
        }
    }
}

fn wait_child_exit(child: &mut Child) -> Result<Option<i32>> {
    let started = Instant::now();
    while started.elapsed() < CHILD_WAIT_TIMEOUT {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.code()),
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(source) => {
                return Err(err_with_source(error_info::WATCH_EVENT_FAILED, source));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Clone)]
struct StartSpec {
    name: String,
    command: String,
    kind: SessionKind,
    launch: SessionLaunch,
    remote_control: Option<RemoteSessionControl>,
    conflict: StartConflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartConflict {
    RejectExisting,
    ReplaceInactive,
}

impl StartSpec {
    fn local(name: String, command: String) -> Self {
        Self {
            name,
            command,
            kind: SessionKind::Local,
            launch: SessionLaunch::Local,
            remote_control: None,
            conflict: StartConflict::RejectExisting,
        }
    }

    fn remote(spec: RemoteSessionSpec) -> Self {
        let remote = RemoteSession {
            host: spec.host.clone(),
            port: spec.port,
            remote_root: spec.remote_root.clone(),
        };
        Self {
            name: spec.name.clone(),
            command: spec.command.clone(),
            kind: SessionKind::Remote,
            remote_control: Some(remote.control(&spec.name)),
            launch: SessionLaunch::Remote(spec),
            conflict: StartConflict::RejectExisting,
        }
    }

    fn replace_inactive(mut self) -> Self {
        self.conflict = StartConflict::ReplaceInactive;
        self
    }
}

impl SessionStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Stopped => "stopped",
        }
    }
}

impl SessionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

pub fn parse_console_command(line: &str) -> ConsoleCommand {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ConsoleCommand::Empty;
    }
    if matches!(trimmed, "help" | "?") {
        return ConsoleCommand::Help;
    }
    if matches!(trimmed, "ps" | "sessions") {
        return ConsoleCommand::Sessions;
    }
    if matches!(trimmed, "quit!" | "exit!") {
        return ConsoleCommand::QuitForce;
    }
    if matches!(trimmed, "quit" | "exit") {
        return ConsoleCommand::Quit;
    }
    if trimmed == "s" {
        return ConsoleCommand::StopFocused;
    }
    if trimmed == "r" {
        return ConsoleCommand::RestartFocused;
    }
    if trimmed == "sync" {
        return ConsoleCommand::Sync;
    }
    if matches!(trimmed, "saved-sessions" | "saved sessions") {
        return ConsoleCommand::SavedSessions;
    }
    if let Some(rest) = trimmed.strip_prefix("restore ") {
        return ConsoleCommand::RestoreSession {
            selector: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("delete session ") {
        return ConsoleCommand::DeleteSavedSession {
            selector: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("logs") {
        let selector = optional_arg(rest);
        return ConsoleCommand::Logs { selector };
    }
    if let Some(rest) = trimmed.strip_prefix("tail") {
        return parse_tail(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("clear-logs") {
        let selector = optional_arg(rest);
        return ConsoleCommand::ClearLogs { selector };
    }
    if let Some(rest) = trimmed.strip_prefix("focus ") {
        return ConsoleCommand::Focus {
            selector: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("stop ") {
        return ConsoleCommand::Stop {
            selector: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("restart ") {
        return ConsoleCommand::Restart {
            selector: rest.trim().to_owned(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("new session ") {
        return parse_new_session(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("new remote-session ") {
        return parse_new_remote_session(rest);
    }
    ConsoleCommand::Unknown(trimmed.to_owned())
}

pub fn help_text() -> &'static str {
    "Commands\n\
     sessions|ps                 list runtime sessions\n\
     saved-sessions              list saved session templates\n\
     new session <name> -- <cmd> create or replace an inactive local session\n\
     new remote-session <name> -- <cmd>\n\
                                 create or replace an inactive remote session\n\
     restore <name|index>        start a saved session template\n\
     delete session <name|index> delete saved template and inactive runtime session\n\
     logs [name|id]              show logs for a session\n\
     tail [name|id] [lines]      show recent session logs\n\
     clear-logs [name|id]        clear buffered logs\n\
     focus <name|id>             focus a session\n\
     stop <name|id> | s          stop a session, or focused session with s\n\
     restart <name|id> | r       restart a session, or focused session with r\n\
     sync                        run full sync\n\
     quit / quit!                exit, or force stop sessions and exit\n\
     Keys\n\
     Ctrl+1..9                   focus process by number\n\
     Ctrl+C                      copy selection; cancel sync only when sync focused\n\
     Ctrl+Up/Down                focus previous/next process\n\
     PageUp/PageDown             scroll current logs\n\
     Esc                         clear input and selection"
}

fn parse_new_session(rest: &str) -> ConsoleCommand {
    let Some((name, command)) = rest.split_once(" -- ") else {
        return ConsoleCommand::Unknown(
            "new session requires: new session <name> -- <command>".to_owned(),
        );
    };
    let name = name.trim();
    let command = command.trim();
    if name.is_empty() || command.is_empty() {
        return ConsoleCommand::Unknown("new session requires name and command".to_owned());
    }
    ConsoleCommand::NewSession {
        name: name.to_owned(),
        command: command.to_owned(),
    }
}

fn parse_new_remote_session(rest: &str) -> ConsoleCommand {
    let Some((name, command)) = rest.split_once(" -- ") else {
        return ConsoleCommand::Unknown(
            "new remote-session requires: new remote-session <name> -- <command>".to_owned(),
        );
    };
    let name = name.trim();
    let command = command.trim();
    if name.is_empty() || command.is_empty() {
        return ConsoleCommand::Unknown("new remote-session requires name and command".to_owned());
    }
    ConsoleCommand::NewRemoteSession {
        name: name.to_owned(),
        command: command.to_owned(),
    }
}

fn optional_arg(rest: &str) -> Option<String> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn parse_tail(rest: &str) -> ConsoleCommand {
    let args = rest.split_whitespace().collect::<Vec<_>>();
    match args.as_slice() {
        [] => ConsoleCommand::Tail {
            selector: None,
            lines: 50,
        },
        [single] => match single.parse::<usize>() {
            Ok(lines) => ConsoleCommand::Tail {
                selector: None,
                lines,
            },
            Err(_) => ConsoleCommand::Tail {
                selector: Some((*single).to_owned()),
                lines: 50,
            },
        },
        [selector, lines] => match lines.parse::<usize>() {
            Ok(lines) => ConsoleCommand::Tail {
                selector: Some((*selector).to_owned()),
                lines,
            },
            Err(_) => ConsoleCommand::Unknown("tail usage: tail [name|id] [lines]".to_owned()),
        },
        _ => ConsoleCommand::Unknown("tail usage: tail [name|id] [lines]".to_owned()),
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut shell = Command::new("powershell");
        shell.arg("-NoProfile").arg("-Command").arg(command);
        shell
    }
    #[cfg(not(windows))]
    {
        let mut shell = Command::new("sh");
        shell.arg("-lc").arg(command);
        shell
    }
}

#[derive(Debug, Clone)]
struct RemoteSession {
    host: String,
    port: u16,
    remote_root: String,
}

impl RemoteSession {
    fn shell_command(&self, name: &str, command: &str) -> Command {
        let remote_command = self.remote_session_command(name, command);
        let remote_shell = remote_session_shell(&remote_command);
        let mut ssh = Command::new("ssh");
        ssh.arg("-n")
            .arg("-T")
            .arg("-p")
            .arg(self.port.to_string())
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg(self.host.clone())
            .arg(remote_shell);
        ssh
    }

    fn control(&self, name: &str) -> RemoteSessionControl {
        RemoteSessionControl {
            host: self.host.clone(),
            port: self.port,
            pid_file: format!("{}/.rdev/sessions/{}.pid", self.remote_root, name),
        }
    }

    fn remote_session_command(&self, name: &str, command: &str) -> String {
        let control = self.control(name);
        format!(
            "cd {root} && mkdir -p .rdev/sessions && pid_file={pid_file}; rm -f \"$pid_file\"; \
             export RDEV_SESSION_PID_FILE=\"$pid_file\"; \
             export RDEV_SESSION_COMMAND={session_command}; \
             if command -v setsid >/dev/null 2>&1; then exec setsid {shell} -lc {runner}; else exec {shell} -lc {runner}; fi",
            root = shell_quote(&self.remote_root),
            pid_file = shell_quote(&control.pid_file),
            shell = REMOTE_SESSION_SHELL,
            session_command = shell_quote(&remote_user_command(command)),
            runner = shell_quote(REMOTE_SESSION_RUNNER),
        )
    }
}

fn terminate_remote_session(control: &RemoteSessionControl) -> Result<()> {
    let stop_command = remote_stop_command(control);
    let remote_shell = remote_session_shell(&stop_command);
    let output = Command::new("ssh")
        .arg("-n")
        .arg("-T")
        .arg("-p")
        .arg(control.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(control.host.clone())
        .arg(remote_shell)
        .output()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(err(error_info::SESSION_FAILED)
            .with_hint(String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn remote_session_shell(script: &str) -> String {
    format!("{REMOTE_SESSION_SHELL} -lc {}", shell_quote(script))
}

fn remote_user_command(command: &str) -> String {
    format!("{REMOTE_SESSION_ENV}; {command}")
}

fn remote_stop_command(control: &RemoteSessionControl) -> String {
    format!(
        "pid_file={pid_file}; \
         if [ -s \"$pid_file\" ]; then \
           pid=$(cat \"$pid_file\"); \
           kill -INT -\"$pid\" 2>/dev/null || kill -INT \"$pid\" 2>/dev/null || true; \
           sleep 1; \
           kill -TERM -\"$pid\" 2>/dev/null || kill -TERM \"$pid\" 2>/dev/null || true; \
           sleep 1; \
           kill -KILL -\"$pid\" 2>/dev/null || kill -KILL \"$pid\" 2>/dev/null || true; \
           rm -f \"$pid_file\"; \
         fi",
        pid_file = shell_quote(&control.pid_file),
    )
}

struct LogReader<R> {
    shared: SharedSessions,
    id: u32,
    stream: &'static str,
    reader: Option<R>,
}

fn spawn_log_reader<R>(task: LogReader<R>)
where
    R: std::io::Read + Send + 'static,
{
    if let Some(reader) = task.reader {
        thread::spawn(move || {
            let reader = BufReader::new(reader);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => format!("[rdev] {} read error: {error}", task.stream),
                };
                if let Ok(mut manager) = task.shared.lock() {
                    manager.push_log(task.id, format!("[{}] {line}", task.stream));
                }
            }
        });
    }
}

fn lock_sessions(shared: &SharedSessions) -> Result<std::sync::MutexGuard<'_, SessionManager>> {
    shared
        .lock()
        .map_err(|_| err(error_info::SESSION_FAILED).with_hint("session manager poisoned"))
}

fn validate_session_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if valid {
        Ok(())
    } else {
        Err(err(error_info::SESSION_FAILED)
            .with_hint("session name may only contain ascii letters, numbers, _, -, ."))
    }
}

fn push_log(session: &mut ManagedSession, line: String) {
    if session.logs.len() >= MAX_LOG_LINES {
        session.logs.pop_front();
    }
    session.logs.push_back(line);
}

fn command_display(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(
        command
            .get_args()
            .map(|arg| quote_display_arg(&arg.to_string_lossy())),
    );
    parts.join(" ")
}

fn quote_display_arg(arg: &str) -> String {
    if arg.is_empty() || arg.chars().any(char::is_whitespace) {
        format!("{arg:?}")
    } else {
        arg.to_owned()
    }
}

fn status_code_label(code: Option<i32>) -> String {
    code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
}

#[cfg(windows)]
fn terminate_child(child: &mut Child) -> Result<()> {
    let output = Command::new("taskkill")
        .arg("/PID")
        .arg(child.id().to_string())
        .arg("/T")
        .arg("/F")
        .output()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if output.status.success() {
        Ok(())
    } else {
        child
            .kill()
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))
    }
}

#[cfg(not(windows))]
fn terminate_child(child: &mut Child) -> Result<()> {
    child
        .kill()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{
        parse_console_command, remote_stop_command, ConsoleCommand, ManagedSession, RemoteSession,
        SessionKind, SessionLaunch, SessionManager, SessionStatus,
    };
    use crate::error_info;

    #[test]
    fn parses_new_session_command() {
        let command = parse_console_command("new session web -- pnpm dev");

        assert_eq!(
            command,
            ConsoleCommand::NewSession {
                name: "web".to_owned(),
                command: "pnpm dev".to_owned()
            }
        );
    }

    #[test]
    fn parses_new_remote_session_command() {
        let command = parse_console_command("new remote-session web -- pnpm preview --host");

        assert_eq!(
            command,
            ConsoleCommand::NewRemoteSession {
                name: "web".to_owned(),
                command: "pnpm preview --host".to_owned()
            }
        );
    }

    #[test]
    fn parses_saved_session_commands() {
        assert_eq!(
            parse_console_command("saved-sessions"),
            ConsoleCommand::SavedSessions
        );
        assert_eq!(
            parse_console_command("restore web"),
            ConsoleCommand::RestoreSession {
                selector: "web".to_owned()
            }
        );
        assert_eq!(
            parse_console_command("delete session web"),
            ConsoleCommand::DeleteSavedSession {
                selector: "web".to_owned()
            }
        );
    }

    #[test]
    fn parses_focused_session_shorthands() {
        assert_eq!(parse_console_command("s"), ConsoleCommand::StopFocused);
        assert_eq!(parse_console_command("r"), ConsoleCommand::RestartFocused);
    }

    #[test]
    fn duplicate_session_name_uses_session_error() {
        let shared = SessionManager::shared(std::env::temp_dir());
        insert_local_session(&shared, "web", SessionStatus::Running);

        let error = match SessionManager::start(&shared, "web".to_owned(), "echo ok".to_owned()) {
            Ok(message) => panic!("duplicate session should fail, got: {message}"),
            Err(error) => error,
        };

        assert_eq!(error.info.code, error_info::SESSION_FAILED.code);
        assert_eq!(error.hint.as_deref(), Some("已存在正在运行的同名会话: web"));
    }

    #[test]
    fn new_session_replaces_exited_session_with_same_name() {
        let shared = SessionManager::shared(std::env::temp_dir());
        insert_local_session(&shared, "web", SessionStatus::Exited);

        let result = SessionManager::start(&shared, "web".to_owned(), "echo new".to_owned());

        assert!(
            result.is_ok(),
            "exited session should be replaceable: {result:?}"
        );
    }

    #[test]
    fn restore_replaces_stopped_session_with_same_name() {
        let shared = SessionManager::shared(std::env::temp_dir());
        insert_local_session(&shared, "web", SessionStatus::Stopped);

        let result = SessionManager::restore(&shared, "web".to_owned(), "echo restored".to_owned());

        assert!(
            result.is_ok(),
            "stopped session should be replaceable: {result:?}"
        );
    }

    #[test]
    fn restore_does_not_replace_running_session_with_same_name() {
        let shared = SessionManager::shared(std::env::temp_dir());
        insert_local_session(&shared, "web", SessionStatus::Running);

        let error =
            match SessionManager::restore(&shared, "web".to_owned(), "echo restored".to_owned()) {
                Ok(message) => panic!("running session should not be replaced, got: {message}"),
                Err(error) => error,
            };

        assert_eq!(error.info.code, error_info::SESSION_FAILED.code);
        assert_eq!(error.hint.as_deref(), Some("已存在正在运行的同名会话: web"));
    }

    #[test]
    fn delete_inactive_session_releases_name() {
        let shared = SessionManager::shared(std::env::temp_dir());
        insert_local_session(&shared, "web", SessionStatus::Exited);

        let message = {
            let mut manager = shared.lock().unwrap_or_else(|error| error.into_inner());
            manager
                .delete_inactive("web")
                .unwrap_or_else(|error| panic!("{error}"))
        };

        assert_eq!(message, "session deleted: web");
        let manager = shared.lock().unwrap_or_else(|error| error.into_inner());
        assert!(!manager.names.contains_key("web"));
    }

    #[test]
    fn delete_inactive_session_rejects_running() {
        let shared = SessionManager::shared(std::env::temp_dir());
        insert_local_session(&shared, "web", SessionStatus::Running);

        let error = {
            let mut manager = shared.lock().unwrap_or_else(|error| error.into_inner());
            match manager.delete_inactive("web") {
                Ok(message) => panic!("running session should not be deleted, got: {message}"),
                Err(error) => error,
            }
        };

        assert_eq!(error.info.code, error_info::SESSION_FAILED.code);
        assert_eq!(error.hint.as_deref(), Some("会话正在运行，不能删除: web"));
    }

    #[test]
    fn parses_logs_without_selector() {
        assert_eq!(
            parse_console_command("logs"),
            ConsoleCommand::Logs { selector: None }
        );
    }

    #[test]
    fn parses_tail_command() {
        assert_eq!(
            parse_console_command("tail web 20"),
            ConsoleCommand::Tail {
                selector: Some("web".to_owned()),
                lines: 20
            }
        );
        assert_eq!(
            parse_console_command("tail 10"),
            ConsoleCommand::Tail {
                selector: None,
                lines: 10
            }
        );
    }

    #[test]
    fn remote_session_runs_compound_command_under_shell() {
        let remote = RemoteSession {
            host: "root@example.test".to_owned(),
            port: 2222,
            remote_root: "/root/project".to_owned(),
        };

        let command = remote.shell_command("web", "cd app && pwd");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(args.iter().any(|arg| arg == "-n"));
        assert!(args.iter().any(|arg| arg == "-T"));
        assert!(args.iter().any(|arg| arg == "ServerAliveInterval=15"));
        assert!(args.iter().any(|arg| arg == "ServerAliveCountMax=3"));
        assert!(args.iter().any(|arg| arg == "root@example.test"));
        let remote_shell = args.last().map(String::as_str).unwrap_or_default();
        assert!(remote_shell.starts_with("bash -lc "));
        assert!(remote_shell.contains("/root/project"));
        assert!(remote_shell.contains(".rdev/sessions/web.pid"));
        assert!(remote_shell.contains("bash -lc"));
        assert!(remote_shell.contains("exec setsid"));
        assert!(remote_shell.contains("RDEV_SESSION_COMMAND"));
        assert!(remote_shell.contains("cd app && pwd"));
        assert!(!remote_shell.contains("exec cd app"));
        assert!(!remote_shell.contains(" & "));
        assert!(!remote_shell.contains("wait \"$child\""));
    }

    #[test]
    fn remote_stop_prefers_interrupt_before_terminate() {
        let remote = RemoteSession {
            host: "root@example.test".to_owned(),
            port: 2222,
            remote_root: "/root/project".to_owned(),
        };
        let stop_command = remote_stop_command(&remote.control("web"));

        let int = stop_command.find("kill -INT").unwrap_or(usize::MAX);
        let term = stop_command.find("kill -TERM").unwrap_or(usize::MAX);
        let kill = stop_command.find("kill -KILL").unwrap_or(usize::MAX);
        assert!(int < term);
        assert!(term < kill);
    }

    #[test]
    fn rejects_unknown_command() {
        assert_eq!(
            parse_console_command("new session web"),
            ConsoleCommand::Unknown(
                "new session requires: new session <name> -- <command>".to_owned()
            )
        );
    }

    fn insert_local_session(shared: &super::SharedSessions, name: &str, status: SessionStatus) {
        let mut manager = shared.lock().unwrap_or_else(|error| error.into_inner());
        let id = manager.next_id;
        manager.next_id += 1;
        manager.names.insert(name.to_owned(), id);
        manager.sessions.insert(
            id,
            ManagedSession {
                id,
                name: name.to_owned(),
                kind: SessionKind::Local,
                launch: SessionLaunch::Local,
                remote_control: None,
                command: "echo old".to_owned(),
                status,
                child: None,
                logs: VecDeque::new(),
                exit_code: None,
            },
        );
    }
}
