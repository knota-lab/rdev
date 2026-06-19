use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cli::{DaemonArgs, DaemonCommand, ExecArgs};
use crate::config::{AppConfig, CONFIG_DIR_NAME};
use crate::error::{err, err_with_source, RdevError, Result};
use crate::error_info;
use crate::exec_summary::ExecSummaryRecorder;
use crate::path::{RelativePath, RemotePath};
use crate::ssh::SshClient;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const DAEMON_FILE: &str = "daemon.json";
const START_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(150);
const FRAME_HEADER_LEN: usize = 4;
const STREAM_BUFFER_LEN: usize = 16 * 1024;
const SUMMARY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const PGID_MARKER: &str = "__RDEV_PGID=";
const DAEMON_BIN_DIR: &str = "bin";

#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DaemonState {
    pid: u32,
    addr: String,
    token: String,
    project_root: PathBuf,
    remote: String,
    started_at_ms: u128,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DaemonRequest {
    Status {
        token: String,
    },
    Stop {
        token: String,
    },
    ExecStart {
        token: String,
        id: String,
        command: String,
        dir: Option<String>,
    },
    Cancel {
        token: String,
        id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DaemonResponse {
    Status {
        pid: u32,
        remote: String,
        busy: bool,
        active_job: Option<String>,
    },
    Stopped,
    Stdout {
        id: String,
        data: String,
    },
    Stderr {
        id: String,
        data: String,
    },
    Exit {
        id: String,
        code: i32,
    },
    Error {
        code: String,
        message: String,
    },
}

struct DaemonRuntime {
    config: AppConfig,
    state: DaemonState,
    ssh: Option<SshClient>,
    active_job: Option<ActiveJob>,
    shutdown: bool,
}

struct ActiveJob {
    id: String,
    cancel: Arc<AtomicBool>,
    cancel_path: String,
}

struct ExecRequest {
    id: String,
    command: String,
    dir: Option<String>,
}

struct StreamContext<'a> {
    config: &'a AppConfig,
    ssh: &'a mut SshClient,
    stream: &'a mut TcpStream,
    request: &'a ExecRequest,
    cancel: Arc<AtomicBool>,
    remote: &'a RemoteCommand,
}

struct RemoteCommand {
    command: String,
    cancel_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatusSnapshot {
    pub running: bool,
    pub pid: Option<u32>,
    pub addr: Option<String>,
    pub remote: Option<String>,
    pub busy: bool,
    pub active_job: Option<String>,
}

pub fn run_daemon_command(args: DaemonArgs, cwd: &Path) -> Result<String> {
    match args.command {
        DaemonCommand::Start => start_daemon(cwd),
        DaemonCommand::Status => daemon_status(cwd),
        DaemonCommand::Stop => stop_daemon(cwd),
        DaemonCommand::Serve => serve_daemon(cwd),
    }
}

pub fn daemon_status_snapshot(cwd: &Path) -> DaemonStatusSnapshot {
    daemon_status_details(cwd).unwrap_or(DaemonStatusSnapshot {
        running: false,
        pid: None,
        addr: None,
        remote: None,
        busy: false,
        active_job: None,
    })
}

pub fn run_exec(args: ExecArgs, cwd: &Path) -> Result<String> {
    let state = ensure_daemon(cwd)?;
    let mut stream = connect_state(&state)?;
    let id = format!("exec-{}", std::process::id());
    let summary = args.summary;
    let command_for_summary = args.command.clone();
    let dir_for_summary = args.dir.as_ref().map(|dir| dir.display().to_string());
    let mut recorder = if summary {
        Some(ExecSummaryRecorder::new(
            cwd,
            command_for_summary.clone(),
            dir_for_summary.clone(),
        )?)
    } else {
        None
    };
    if let Some(recorder) = recorder.as_ref() {
        eprintln!(
            "[summary] capturing output to {}",
            recorder.path().display()
        );
    }
    let summary_started = Instant::now();
    let mut last_summary_heartbeat = summary_started;
    let cancel_state = state.clone();
    let cancel_id = id.clone();
    ctrlc::set_handler(move || {
        eprintln!("[daemon] cancelling remote exec...");
        let _cancel_result = send_cancel(&cancel_state, &cancel_id);
    })
    .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    write_frame(
        &mut stream,
        &DaemonRequest::ExecStart {
            token: state.token,
            id: id.clone(),
            command: args.command,
            dir: args.dir.map(|dir| dir.display().to_string()),
        },
    )?;
    loop {
        let response: DaemonResponse = read_frame(&mut stream)?;
        match response {
            DaemonResponse::Stdout { data, .. } => {
                if let Some(recorder) = recorder.as_mut() {
                    recorder.record(&data)?;
                    maybe_print_summary_heartbeat(
                        recorder,
                        summary_started,
                        &mut last_summary_heartbeat,
                    );
                } else {
                    print!("{data}");
                    io::stdout().flush().map_err(|source| {
                        err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source)
                    })?;
                }
            }
            DaemonResponse::Stderr { data, .. } => {
                if let Some(recorder) = recorder.as_mut() {
                    recorder.record(&data)?;
                    maybe_print_summary_heartbeat(
                        recorder,
                        summary_started,
                        &mut last_summary_heartbeat,
                    );
                } else {
                    eprint!("{data}");
                    io::stderr().flush().map_err(|source| {
                        err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source)
                    })?;
                }
            }
            DaemonResponse::Exit { code, .. } => {
                let summary_text = match recorder.take() {
                    Some(recorder) => Some(recorder.finish(code)?),
                    None => None,
                };
                if code == 0 {
                    return Ok(summary_text.unwrap_or_default());
                }
                if code == 130 {
                    let mut error =
                        err(error_info::DAEMON_EXEC_CANCELLED).with_exit_code(Some(130));
                    if let Some(summary_text) = summary_text {
                        error = error.with_hint(summary_text);
                    }
                    return Err(error);
                }
                let mut error = err(error_info::REMOTE_COMMAND_FAILED).with_exit_code(Some(code));
                if let Some(summary_text) = summary_text {
                    error = error.with_hint(summary_text);
                }
                return Err(error);
            }
            DaemonResponse::Error { code, message } => {
                return Err(err(error_info::DAEMON_FAILED).with_hint(format!("{code}: {message}")));
            }
            DaemonResponse::Status { .. } | DaemonResponse::Stopped => {}
        }
    }
}

fn maybe_print_summary_heartbeat(
    recorder: &ExecSummaryRecorder,
    started: Instant,
    last_heartbeat: &mut Instant,
) {
    if last_heartbeat.elapsed() < SUMMARY_HEARTBEAT_INTERVAL {
        return;
    }
    *last_heartbeat = Instant::now();
    eprintln!(
        "[summary] running elapsed={}s captured={} bytes log={}",
        started.elapsed().as_secs(),
        recorder.byte_count(),
        recorder.path().display()
    );
}

fn start_daemon(cwd: &Path) -> Result<String> {
    if daemon_is_running(cwd) {
        return daemon_status(cwd);
    }
    let exe = daemon_exe(cwd)?;
    let exe_display = exe.display().to_string();
    let mut command = ProcessCommand::new(&exe);
    command
        .arg("daemon")
        .arg("serve")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    command.spawn().map_err(|source| {
        err_with_source(error_info::DAEMON_FAILED, source)
            .with_hint(format!("failed to start daemon executable: {exe_display}"))
    })?;

    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if let Ok(status) = daemon_status(cwd) {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(err(error_info::DAEMON_FAILED).with_hint("daemon did not become ready in time"))
}

fn daemon_is_running(cwd: &Path) -> bool {
    daemon_status_details(cwd).is_ok()
}

fn daemon_exe(cwd: &Path) -> Result<PathBuf> {
    let current = std::env::current_exe()
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    let dir = cwd.join(CONFIG_DIR_NAME).join(DAEMON_BIN_DIR);
    fs::create_dir_all(&dir)
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    let exe_name = daemon_exe_name(&current);
    let daemon = dir.join(exe_name);
    fs::copy(&current, &daemon).map_err(|source| {
        err_with_source(error_info::DAEMON_FAILED, source).with_hint(format!(
            "failed to prepare daemon executable: {} -> {}",
            current.display(),
            daemon.display()
        ))
    })?;
    Ok(daemon)
}

fn daemon_exe_name(current: &Path) -> &'static str {
    if current.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("target" | "cargo-target")
        )
    }) {
        "rdev-dev-daemon.exe"
    } else {
        "rdev-daemon.exe"
    }
}

fn ensure_daemon(cwd: &Path) -> Result<DaemonState> {
    match read_state(cwd).and_then(|state| {
        let mut stream = connect_state(&state)?;
        write_frame(
            &mut stream,
            &DaemonRequest::Status {
                token: state.token.clone(),
            },
        )?;
        let response: DaemonResponse = read_frame(&mut stream)?;
        if matches!(response, DaemonResponse::Error { .. }) {
            return Err(err(error_info::DAEMON_NOT_RUNNING));
        }
        Ok(state)
    }) {
        Ok(state) => Ok(state),
        Err(_) => {
            start_daemon(cwd)?;
            read_state(cwd)
        }
    }
}

fn daemon_status(cwd: &Path) -> Result<String> {
    let status = match daemon_status_details(cwd) {
        Ok(status) => status,
        Err(error) if is_daemon_not_running(&error) => {
            return Ok("[daemon] not running".to_owned());
        }
        Err(error) => return Err(error),
    };
    let pid = status.pid.unwrap_or(0);
    let remote = status.remote.unwrap_or_else(|| "<unknown>".to_owned());
    let addr = status.addr.unwrap_or_else(|| "<unknown>".to_owned());
    let job = status.active_job.unwrap_or_else(|| "<none>".to_owned());
    Ok(format!(
        "[daemon] pid={pid} remote={remote} addr={addr} busy={} active_job={job}",
        status.busy
    ))
}

fn is_daemon_not_running(error: &RdevError) -> bool {
    error.info.code == error_info::DAEMON_NOT_RUNNING.code
}

fn daemon_status_details(cwd: &Path) -> Result<DaemonStatusSnapshot> {
    let state = read_state(cwd)?;
    let mut stream = connect_state(&state)?;
    write_frame(
        &mut stream,
        &DaemonRequest::Status {
            token: state.token.clone(),
        },
    )?;
    match read_frame(&mut stream)? {
        DaemonResponse::Status {
            pid,
            remote,
            busy,
            active_job,
        } => Ok(DaemonStatusSnapshot {
            running: true,
            pid: Some(pid),
            addr: Some(state.addr),
            remote: Some(remote),
            busy,
            active_job,
        }),
        DaemonResponse::Error { code, message } => {
            Err(err(error_info::DAEMON_FAILED).with_hint(format!("{code}: {message}")))
        }
        _ => Err(err(error_info::DAEMON_PROTOCOL_ERROR).with_hint("unexpected status response")),
    }
}

fn stop_daemon(cwd: &Path) -> Result<String> {
    let state = read_state(cwd)?;
    let mut stream = connect_state(&state)?;
    write_frame(&mut stream, &DaemonRequest::Stop { token: state.token })?;
    match read_frame(&mut stream)? {
        DaemonResponse::Stopped => Ok("[daemon] stopped".to_owned()),
        DaemonResponse::Error { code, message } => {
            Err(err(error_info::DAEMON_FAILED).with_hint(format!("{code}: {message}")))
        }
        _ => Err(err(error_info::DAEMON_PROTOCOL_ERROR).with_hint("unexpected stop response")),
    }
}

fn send_cancel(state: &DaemonState, id: &str) -> Result<()> {
    let mut stream = connect_state(state)?;
    write_frame(
        &mut stream,
        &DaemonRequest::Cancel {
            token: state.token.clone(),
            id: id.to_owned(),
        },
    )
}

fn serve_daemon(cwd: &Path) -> Result<String> {
    let _ignore_ctrl_c = ctrlc::set_handler(|| {});
    let config = AppConfig::load_from_dir(cwd)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    listener
        .set_nonblocking(true)
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    let state = DaemonState {
        pid: std::process::id(),
        addr: listener
            .local_addr()
            .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?
            .to_string(),
        token: new_token(),
        project_root: cwd.to_path_buf(),
        remote: format!("{}:{}", config.remote.host, config.remote.port),
        started_at_ms: now_ms(),
    };
    write_state(cwd, &state)?;
    let ssh = SshClient::connect(&config)?;
    let runtime = Arc::new(Mutex::new(DaemonRuntime {
        config,
        state,
        ssh: Some(ssh),
        active_job: None,
        shutdown: false,
    }));

    loop {
        let should_shutdown = {
            let guard = runtime
                .lock()
                .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
            guard.shutdown && guard.active_job.is_none()
        };
        if should_shutdown {
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let runtime = Arc::clone(&runtime);
                thread::spawn(move || {
                    let _result = handle_client(runtime, stream);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(source) => return Err(err_with_source(error_info::DAEMON_FAILED, source)),
        }
    }
    let _cleanup = fs::remove_file(state_path(cwd));
    Ok("[daemon] stopped".to_owned())
}

fn handle_client(runtime: Arc<Mutex<DaemonRuntime>>, mut stream: TcpStream) -> Result<()> {
    let request: DaemonRequest = read_frame(&mut stream)?;
    let token = request_token(&request);
    {
        let guard = runtime
            .lock()
            .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
        if token != guard.state.token {
            write_frame(
                &mut stream,
                &DaemonResponse::Error {
                    code: error_info::DAEMON_PROTOCOL_ERROR.code.to_owned(),
                    message: "invalid token".to_owned(),
                },
            )?;
            return Ok(());
        }
    }

    match request {
        DaemonRequest::Status { .. } => {
            let guard = runtime
                .lock()
                .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
            write_frame(
                &mut stream,
                &DaemonResponse::Status {
                    pid: guard.state.pid,
                    remote: guard.state.remote.clone(),
                    busy: guard.active_job.is_some(),
                    active_job: guard.active_job.as_ref().map(|job| job.id.clone()),
                },
            )
        }
        DaemonRequest::Stop { .. } => {
            let mut guard = runtime
                .lock()
                .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
            if let Some(job) = &guard.active_job {
                job.cancel.store(true, Ordering::SeqCst);
                write_remote_cancel_file_detached(&guard.config, &job.cancel_path);
            }
            guard.shutdown = true;
            write_frame(&mut stream, &DaemonResponse::Stopped)
        }
        DaemonRequest::ExecStart {
            id, command, dir, ..
        } => {
            let request = ExecRequest { id, command, dir };
            if let Err(error) = exec_request(Arc::clone(&runtime), &mut stream, request) {
                let _send_error = write_frame(
                    &mut stream,
                    &DaemonResponse::Error {
                        code: error.info.code.to_owned(),
                        message: error.to_string(),
                    },
                );
            }
            Ok(())
        }
        DaemonRequest::Cancel { id, .. } => {
            let guard = runtime
                .lock()
                .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
            if let Some(job) = &guard.active_job {
                if job.id == id {
                    job.cancel.store(true, Ordering::SeqCst);
                    write_remote_cancel_file_detached(&guard.config, &job.cancel_path);
                }
            }
            Ok(())
        }
    }
}

fn exec_request(
    runtime: Arc<Mutex<DaemonRuntime>>,
    stream: &mut TcpStream,
    request: ExecRequest,
) -> Result<()> {
    let cancel = Arc::new(AtomicBool::new(false));
    let config = {
        let guard = runtime
            .lock()
            .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
        guard.config.clone()
    };
    let remote = build_remote_command(&config, &request)?;
    {
        let mut guard = runtime
            .lock()
            .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
        if guard.active_job.is_some() {
            write_frame(
                stream,
                &DaemonResponse::Error {
                    code: error_info::DAEMON_BUSY.code.to_owned(),
                    message: "another exec is running".to_owned(),
                },
            )?;
            return Ok(());
        }
        if guard.ssh.is_none() {
            write_frame(
                stream,
                &DaemonResponse::Error {
                    code: error_info::DAEMON_BUSY.code.to_owned(),
                    message: "ssh connection is busy".to_owned(),
                },
            )?;
            return Ok(());
        }
        guard.active_job = Some(ActiveJob {
            id: request.id.clone(),
            cancel: Arc::clone(&cancel),
            cancel_path: remote.cancel_path.clone(),
        });
    }
    let mut ssh = {
        let mut guard = runtime
            .lock()
            .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
        let Some(ssh) = guard.ssh.take() else {
            return Err(err(error_info::DAEMON_BUSY).with_hint("ssh connection is busy"));
        };
        ssh
    };
    let result = stream_remote_command(StreamContext {
        config: &config,
        ssh: &mut ssh,
        stream,
        request: &request,
        cancel: Arc::clone(&cancel),
        remote: &remote,
    });
    let mut guard = runtime
        .lock()
        .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
    guard.ssh = Some(ssh);
    guard.active_job = None;
    result
}

fn stream_remote_command(context: StreamContext<'_>) -> Result<()> {
    let mut channel =
        open_remote_channel_with_retry(context.config, context.ssh, &context.remote.command)?;
    let session = context.ssh.session_mut();
    stream_open_channel(StreamOpenContext {
        config: context.config,
        session,
        stream: context.stream,
        request: context.request,
        cancel: context.cancel,
        remote: context.remote,
        channel: &mut channel,
    })
}

fn open_remote_channel_with_retry(
    config: &AppConfig,
    ssh: &mut SshClient,
    command: &str,
) -> Result<ssh2::Channel> {
    match open_remote_channel(ssh.session_mut(), command) {
        Ok(channel) => Ok(channel),
        Err(first_error) => {
            *ssh = SshClient::connect(config)?;
            open_remote_channel(ssh.session_mut(), command).map_err(|error| {
                error.with_hint(format!(
                    "retried once after SSH channel open failed; first error: {first_error}"
                ))
            })
        }
    }
}

fn open_remote_channel(session: &mut ssh2::Session, command: &str) -> Result<ssh2::Channel> {
    session.set_blocking(true);
    let mut channel = session
        .channel_session()
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    channel
        .exec(command)
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    session.set_blocking(false);
    Ok(channel)
}

struct StreamOpenContext<'a> {
    config: &'a AppConfig,
    session: &'a mut ssh2::Session,
    stream: &'a mut TcpStream,
    request: &'a ExecRequest,
    cancel: Arc<AtomicBool>,
    remote: &'a RemoteCommand,
    channel: &'a mut ssh2::Channel,
}

fn stream_open_channel(context: StreamOpenContext<'_>) -> Result<()> {
    let mut stdout = vec![0_u8; STREAM_BUFFER_LEN];
    let mut stderr = vec![0_u8; STREAM_BUFFER_LEN];
    let mut stderr_tail = String::new();
    let mut pgid: Option<String> = None;
    let mut cancel_sent = false;
    loop {
        if client_disconnected(context.stream) {
            cancel_after_client_disconnect(
                context.config,
                context.remote.cancel_path.as_str(),
                context.channel,
            );
            return Ok(());
        }
        if context.cancel.load(Ordering::SeqCst) && !cancel_sent {
            cancel_sent = true;
            let _cancel = write_remote_cancel_file(context.session, &context.remote.cancel_path);
        }
        let mut progressed = false;
        match context.channel.read(&mut stdout) {
            Ok(0) => {}
            Ok(read) => {
                progressed = true;
                if write_frame(
                    context.stream,
                    &DaemonResponse::Stdout {
                        id: context.request.id.clone(),
                        data: String::from_utf8_lossy(&stdout[..read]).to_string(),
                    },
                )
                .is_err()
                {
                    cancel_after_client_disconnect(
                        context.config,
                        context.remote.cancel_path.as_str(),
                        context.channel,
                    );
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(source) => return Err(err_with_source(error_info::REMOTE_COMMAND_FAILED, source)),
        }
        {
            let mut stderr_stream = context.channel.stderr();
            match stderr_stream.read(&mut stderr) {
                Ok(0) => {}
                Ok(read) => {
                    progressed = true;
                    for data in filter_stderr_chunk(
                        &String::from_utf8_lossy(&stderr[..read]),
                        &mut stderr_tail,
                        &mut pgid,
                    ) {
                        if write_frame(
                            context.stream,
                            &DaemonResponse::Stderr {
                                id: context.request.id.clone(),
                                data,
                            },
                        )
                        .is_err()
                        {
                            cancel_after_client_disconnect(
                                context.config,
                                context.remote.cancel_path.as_str(),
                                context.channel,
                            );
                            return Ok(());
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) => {
                    return Err(err_with_source(error_info::REMOTE_COMMAND_FAILED, source));
                }
            }
        }
        if context.channel.eof() {
            break;
        }
        if !progressed {
            thread::sleep(Duration::from_millis(20));
        }
    }
    context.session.set_blocking(true);
    if let Some(data) = flush_stderr_tail(&mut stderr_tail, &mut pgid) {
        write_frame(
            context.stream,
            &DaemonResponse::Stderr {
                id: context.request.id.clone(),
                data,
            },
        )?;
    }
    context
        .channel
        .wait_close()
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    let code = if cancel_sent {
        130
    } else {
        context.channel.exit_status().unwrap_or(1)
    };
    write_frame(
        context.stream,
        &DaemonResponse::Exit {
            id: context.request.id.clone(),
            code,
        },
    )
}

fn cancel_after_client_disconnect(
    config: &AppConfig,
    cancel_path: &str,
    channel: &mut ssh2::Channel,
) {
    write_remote_cancel_file_detached(config, cancel_path);
    let _closed = channel.close();
}

fn client_disconnected(stream: &TcpStream) -> bool {
    let mut buf = [0_u8; 1];
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let disconnected = match stream.peek(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(_) => true,
    };
    let _restore = stream.set_nonblocking(false);
    disconnected
}

fn build_remote_command(config: &AppConfig, request: &ExecRequest) -> Result<RemoteCommand> {
    let remote_root = RemotePath::parse(config.remote.path.as_str())?;
    let relative = match request.dir.as_deref() {
        Some(dir) => RelativePath::parse(dir)?,
        None => RelativePath::parse(".")?,
    };
    let remote_dir = remote_root.join_relative(&relative);
    let cancel_path = format!(
        "/tmp/rdev-cancel-{}-{}",
        std::process::id(),
        sanitize_job_id(&request.id)
    );
    let runner = remote_exec_wrapper(&request.command, &cancel_path);
    let command = format!(
        "cd {} && exec sh -lc {}",
        shell_quote(remote_dir.as_str()),
        shell_quote(&runner)
    );
    Ok(RemoteCommand {
        command,
        cancel_path,
    })
}

fn remote_exec_wrapper(command: &str, cancel_path: &str) -> String {
    format!(
        r#"cancel_path={cancel_path}
rm -f "$cancel_path"
wrapper_pid=$$
set -- $(ps -o ppid= -p "$wrapper_pid" 2>/dev/null)
original_ppid=$1
if command -v setsid >/dev/null 2>&1; then
  setsid sh -lc {command} </dev/null &
else
  sh -lc {command} </dev/null &
fi
child=$!
echo {PGID_MARKER}$child >&2
(
  while [ ! -e "$cancel_path" ]; do
    set -- $(ps -o ppid= -p "$wrapper_pid" 2>/dev/null)
    current_ppid=$1
    if [ -n "$original_ppid" ] && [ -n "$current_ppid" ] && [ "$current_ppid" != "$original_ppid" ]; then
      break
    fi
    sleep 1
  done
  if kill -0 "$child" 2>/dev/null || kill -0 -"$child" 2>/dev/null; then
    kill -INT -"$child" 2>/dev/null || kill -INT "$child" 2>/dev/null || true
    sleep 1
    kill -TERM -"$child" 2>/dev/null || kill -TERM "$child" 2>/dev/null || true
    sleep 1
    kill -KILL -"$child" 2>/dev/null || kill -KILL "$child" 2>/dev/null || true
  fi
) &
watcher=$!
wait "$child"
status=$?
kill "$watcher" 2>/dev/null || true
wait "$watcher" 2>/dev/null || true
rm -f "$cancel_path"
exit "$status""#,
        cancel_path = shell_quote(cancel_path),
        command = shell_quote(command)
    )
}

fn write_remote_cancel_file(session: &mut ssh2::Session, cancel_path: &str) -> Result<()> {
    session.set_blocking(true);
    let sftp = session
        .sftp()
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    let mut file = sftp
        .create(Path::new(cancel_path))
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    file.write_all(b"cancel\n")
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    session.set_blocking(false);
    Ok(())
}

fn write_remote_cancel_file_detached(config: &AppConfig, cancel_path: &str) {
    let config = config.clone();
    let cancel_path = cancel_path.to_owned();
    thread::spawn(move || {
        let _cancel = SshClient::connect(&config)
            .and_then(|mut ssh| write_remote_cancel_file(ssh.session_mut(), &cancel_path));
    });
}

fn sanitize_job_id(id: &str) -> String {
    id.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn filter_stderr_chunk(chunk: &str, tail: &mut String, pgid: &mut Option<String>) -> Vec<String> {
    tail.push_str(chunk);
    let mut output = Vec::new();
    while let Some(newline) = tail.find('\n') {
        let line = tail[..=newline].to_owned();
        let rest = tail[newline + 1..].to_owned();
        *tail = rest;
        if let Some(value) = line.trim().strip_prefix(PGID_MARKER) {
            *pgid = Some(value.to_owned());
        } else {
            output.push(line);
        }
    }
    output
}

fn flush_stderr_tail(tail: &mut String, pgid: &mut Option<String>) -> Option<String> {
    if tail.is_empty() {
        return None;
    }
    let line = std::mem::take(tail);
    if let Some(value) = line.trim().strip_prefix(PGID_MARKER) {
        *pgid = Some(value.to_owned());
        None
    } else {
        Some(line)
    }
}

fn read_state(cwd: &Path) -> Result<DaemonState> {
    let path = state_path(cwd);
    let raw = fs::read_to_string(&path)
        .map_err(|source| err_with_source(error_info::DAEMON_NOT_RUNNING, source))?;
    serde_json::from_str(&raw).map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))
}

fn write_state(cwd: &Path, state: &DaemonState) -> Result<()> {
    let dir = cwd.join(CONFIG_DIR_NAME);
    fs::create_dir_all(&dir)
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    let raw = serde_json::to_string_pretty(state)
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    fs::write(state_path(cwd), raw)
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))
}

fn state_path(cwd: &Path) -> PathBuf {
    cwd.join(CONFIG_DIR_NAME).join(DAEMON_FILE)
}

fn connect_state(state: &DaemonState) -> Result<TcpStream> {
    let addr = state
        .addr
        .to_socket_addrs()
        .map_err(|source| err_with_source(error_info::DAEMON_NOT_RUNNING, source))?
        .next()
        .ok_or_else(|| {
            err(error_info::DAEMON_NOT_RUNNING).with_hint("daemon address is invalid")
        })?;
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|source| err_with_source(error_info::DAEMON_NOT_RUNNING, source))
}

fn request_token(request: &DaemonRequest) -> &str {
    match request {
        DaemonRequest::Status { token }
        | DaemonRequest::Stop { token }
        | DaemonRequest::Cancel { token, .. }
        | DaemonRequest::ExecStart { token, .. } => token,
    }
}

fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| err(error_info::DAEMON_PROTOCOL_ERROR).with_hint("frame is too large"))?;
    stream
        .write_all(&len.to_be_bytes())
        .and_then(|()| stream.write_all(&payload))
        .map_err(|source| err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source))
}

fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    let mut header = [0_u8; FRAME_HEADER_LEN];
    stream
        .read_exact(&mut header)
        .map_err(|source| err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source))?;
    let len = u32::from_be_bytes(header) as usize;
    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|source| err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source))?;
    serde_json::from_slice(&payload)
        .map_err(|source| err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source))
}

fn new_token() -> String {
    format!("{}-{}", std::process::id(), now_ms())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{daemon_exe_name, daemon_is_running, daemon_status};

    #[test]
    fn daemon_exe_name_marks_target_build_as_dev_daemon() {
        assert_eq!(
            daemon_exe_name(Path::new(r"J:\cargo-target\release\rdev.exe")),
            "rdev-dev-daemon.exe"
        );
        assert_eq!(
            daemon_exe_name(Path::new(
                r"J:\RustWorkspace\rdev-workspace\rdev\target\debug\rdev.exe"
            )),
            "rdev-dev-daemon.exe"
        );
    }

    #[test]
    fn daemon_exe_name_marks_non_target_binary_as_daemon() {
        assert_eq!(
            daemon_exe_name(Path::new(r"C:\Users\11989\.cargo\bin\rdev.exe")),
            "rdev-daemon.exe"
        );
    }

    #[test]
    fn daemon_status_reports_not_running_without_state_file() {
        let root = std::env::temp_dir().join(format!(
            "rdev-daemon-status-missing-state-{}",
            std::process::id()
        ));
        let _cleanup_before = std::fs::remove_dir_all(&root);
        if let Err(error) = std::fs::create_dir_all(&root) {
            panic!("create dir: {error}");
        }

        let status = match daemon_status(&root) {
            Ok(status) => status,
            Err(error) => panic!("daemon status should be printable: {error}"),
        };

        assert_eq!(status, "[daemon] not running");
        let _cleanup_after = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_state_is_not_running() {
        let root = std::env::temp_dir().join(format!(
            "rdev-daemon-running-missing-state-{}",
            std::process::id()
        ));
        let _cleanup_before = std::fs::remove_dir_all(&root);
        if let Err(error) = std::fs::create_dir_all(&root) {
            panic!("create dir: {error}");
        }

        assert!(!daemon_is_running(&root));
        let _cleanup_after = std::fs::remove_dir_all(&root);
    }
}
