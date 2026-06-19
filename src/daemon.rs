use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cli::{DaemonArgs, DaemonCommand, ExecArgs};
use crate::config::{AppConfig, CONFIG_DIR_NAME};
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::{RelativePath, RemotePath};
use crate::ssh::SshClient;

const DAEMON_FILE: &str = "daemon.json";
const START_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_HEADER_LEN: usize = 4;
const STREAM_BUFFER_LEN: usize = 16 * 1024;

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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DaemonResponse {
    Status {
        pid: u32,
        remote: String,
        busy: bool,
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
    ssh: SshClient,
    busy: bool,
    shutdown: bool,
}

struct ExecRequest {
    id: String,
    command: String,
    dir: Option<String>,
}

pub fn run_daemon_command(args: DaemonArgs, cwd: &Path) -> Result<String> {
    match args.command {
        DaemonCommand::Start => start_daemon(cwd),
        DaemonCommand::Status => daemon_status(cwd),
        DaemonCommand::Stop => stop_daemon(cwd),
        DaemonCommand::Serve => serve_daemon(cwd),
    }
}

pub fn run_exec(args: ExecArgs, cwd: &Path) -> Result<String> {
    let state = ensure_daemon(cwd)?;
    let mut stream = connect_state(&state)?;
    let id = format!("exec-{}", std::process::id());
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
                print!("{data}");
                io::stdout()
                    .flush()
                    .map_err(|source| err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source))?;
            }
            DaemonResponse::Stderr { data, .. } => {
                eprint!("{data}");
                io::stderr()
                    .flush()
                    .map_err(|source| err_with_source(error_info::DAEMON_PROTOCOL_ERROR, source))?;
            }
            DaemonResponse::Exit { code, .. } => {
                if code == 0 {
                    return Ok(String::new());
                }
                return Err(err(error_info::REMOTE_COMMAND_FAILED).with_exit_code(Some(code)));
            }
            DaemonResponse::Error { code, message } => {
                return Err(err(error_info::DAEMON_FAILED).with_hint(format!("{code}: {message}")));
            }
            DaemonResponse::Status { .. } | DaemonResponse::Stopped => {}
        }
    }
}

fn start_daemon(cwd: &Path) -> Result<String> {
    if let Ok(status) = daemon_status(cwd) {
        return Ok(status);
    }
    let exe = std::env::current_exe()
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
    ProcessCommand::new(exe)
        .arg("daemon")
        .arg("serve")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;

    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if let Ok(status) = daemon_status(cwd) {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(err(error_info::DAEMON_FAILED).with_hint("daemon did not become ready in time"))
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
        let _: DaemonResponse = read_frame(&mut stream)?;
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
    let state = read_state(cwd)?;
    let mut stream = connect_state(&state)?;
    write_frame(
        &mut stream,
        &DaemonRequest::Status {
            token: state.token.clone(),
        },
    )?;
    match read_frame(&mut stream)? {
        DaemonResponse::Status { pid, remote, busy } => Ok(format!(
            "[daemon] pid={pid} remote={remote} addr={} busy={busy}",
            state.addr
        )),
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

fn serve_daemon(cwd: &Path) -> Result<String> {
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
        ssh,
        busy: false,
        shutdown: false,
    }));

    loop {
        if runtime
            .lock()
            .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?
            .shutdown
        {
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
                    busy: guard.busy,
                },
            )
        }
        DaemonRequest::Stop { .. } => {
            let mut guard = runtime
                .lock()
                .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
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
    }
}

fn exec_request(
    runtime: Arc<Mutex<DaemonRuntime>>,
    stream: &mut TcpStream,
    request: ExecRequest,
) -> Result<()> {
    let mut guard = runtime
        .lock()
        .map_err(|_| err(error_info::DAEMON_FAILED).with_hint("daemon lock poisoned"))?;
    if guard.busy {
        write_frame(
            stream,
            &DaemonResponse::Error {
                code: error_info::DAEMON_BUSY.code.to_owned(),
                message: "another exec is running".to_owned(),
            },
        )?;
        return Ok(());
    }
    guard.busy = true;
    let result = stream_remote_command(&mut guard, stream, &request);
    guard.busy = false;
    result
}

fn stream_remote_command(
    runtime: &mut DaemonRuntime,
    stream: &mut TcpStream,
    request: &ExecRequest,
) -> Result<()> {
    let remote_command =
        build_remote_command(&runtime.config, &request.command, request.dir.as_deref())?;
    let session = runtime.ssh.session_mut();
    session.set_blocking(true);
    let mut channel = session
        .channel_session()
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    channel
        .exec(&remote_command)
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    session.set_blocking(false);

    let mut stdout = vec![0_u8; STREAM_BUFFER_LEN];
    let mut stderr = vec![0_u8; STREAM_BUFFER_LEN];
    loop {
        let mut progressed = false;
        match channel.read(&mut stdout) {
            Ok(0) => {}
            Ok(read) => {
                progressed = true;
                if write_frame(
                    stream,
                    &DaemonResponse::Stdout {
                        id: request.id.clone(),
                        data: String::from_utf8_lossy(&stdout[..read]).to_string(),
                    },
                )
                .is_err()
                {
                    let _closed = channel.close();
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(source) => return Err(err_with_source(error_info::REMOTE_COMMAND_FAILED, source)),
        }
        {
            let mut stderr_stream = channel.stderr();
            match stderr_stream.read(&mut stderr) {
                Ok(0) => {}
                Ok(read) => {
                    progressed = true;
                    if write_frame(
                        stream,
                        &DaemonResponse::Stderr {
                            id: request.id.clone(),
                            data: String::from_utf8_lossy(&stderr[..read]).to_string(),
                        },
                    )
                    .is_err()
                    {
                        let _closed = channel.close();
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) => {
                    return Err(err_with_source(error_info::REMOTE_COMMAND_FAILED, source));
                }
            }
        }
        if channel.eof() {
            break;
        }
        if !progressed {
            thread::sleep(Duration::from_millis(20));
        }
    }
    session.set_blocking(true);
    channel
        .wait_close()
        .map_err(|source| err_with_source(error_info::REMOTE_COMMAND_FAILED, source))?;
    let code = channel.exit_status().unwrap_or(1);
    write_frame(
        stream,
        &DaemonResponse::Exit {
            id: request.id.clone(),
            code,
        },
    )
}

fn build_remote_command(config: &AppConfig, command: &str, dir: Option<&str>) -> Result<String> {
    let remote_root = RemotePath::parse(config.remote.path.as_str())?;
    let relative = match dir {
        Some(dir) => RelativePath::parse(dir)?,
        None => RelativePath::parse(".")?,
    };
    let remote_dir = remote_root.join_relative(&relative);
    Ok(format!(
        "cd {} && exec sh -lc {}",
        shell_quote(remote_dir.as_str()),
        shell_quote(command)
    ))
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
    TcpStream::connect(&state.addr)
        .map_err(|source| err_with_source(error_info::DAEMON_NOT_RUNNING, source))
}

fn request_token(request: &DaemonRequest) -> &str {
    match request {
        DaemonRequest::Status { token }
        | DaemonRequest::Stop { token }
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
