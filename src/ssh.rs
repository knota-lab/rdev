use std::collections::BTreeSet;
use std::fs::File as FsFile;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssh2::{Session, Sftp};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, ErrorInfo, Result};
use crate::error_info;
use crate::sync_output::{console_output, SyncOutput};

pub(crate) const SSH_IO_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_BASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const UPLOAD_CHUNK_SIZE: usize = 64 * 1024;

pub(crate) struct SshClient {
    session: Session,
    sftp: Option<Sftp>,
    ensured_dirs: BTreeSet<PathBuf>,
    output: Arc<dyn SyncOutput>,
}

pub(crate) struct UploadRequest<'a> {
    pub(crate) local_path: &'a Path,
    pub(crate) remote_path: &'a Path,
    pub(crate) cancelled: Option<&'a Arc<AtomicBool>>,
}

impl SshClient {
    pub(crate) fn connect(config: &AppConfig) -> Result<Self> {
        let endpoint = SshEndpoint::parse(&config.remote.host, config.remote.port);
        let tcp = TcpStream::connect(endpoint.address()).map_err(|source| {
            err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source)
                .with_remote(config.remote.host.clone())
        })?;
        tcp.set_read_timeout(Some(SSH_IO_TIMEOUT))
            .map_err(|source| {
                err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source)
                    .with_remote(config.remote.host.clone())
            })?;
        tcp.set_write_timeout(Some(SSH_IO_TIMEOUT))
            .map_err(|source| {
                err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source)
                    .with_remote(config.remote.host.clone())
            })?;
        let mut session = Session::new()
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?;
        session.set_tcp_stream(tcp);
        session.handshake().map_err(|source| {
            err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source)
                .with_remote(config.remote.host.clone())
        })?;
        authenticate(&session, &endpoint.user, config)?;
        Ok(Self {
            session,
            sftp: None,
            ensured_dirs: BTreeSet::new(),
            output: console_output(),
        })
    }

    pub(crate) fn set_output(&mut self, output: Arc<dyn SyncOutput>) {
        self.output = output;
    }

    pub(crate) fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    pub(crate) fn upload(&mut self, request: UploadRequest<'_>) -> Result<()> {
        if is_cancelled(request.cancelled) {
            return Err(err(error_info::SYNC_CANCELLED));
        }
        if request.local_path.is_dir() {
            return self.ensure_dir(request.remote_path);
        }
        let ensure_started = Instant::now();
        self.ensure_parent_dir(request.remote_path)?;
        let ensure_elapsed = ensure_started.elapsed().as_millis();

        let open_started = Instant::now();
        let mut local = match FsFile::open(request.local_path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                self.output.line(format!(
                    "[sync] upload skipped vanished local={}",
                    request.local_path.display()
                ));
                return Ok(());
            }
            Err(source) => {
                return Err(err_with_source(error_info::SYNC_SFTP_FAILED, source)
                    .with_path(request.local_path.display()));
            }
        };
        let open_elapsed = open_started.elapsed().as_millis();

        let create_started = Instant::now();
        let mut remote = self.create_remote_file(request.remote_path)?;
        let create_elapsed = create_started.elapsed().as_millis();

        let copy_started = Instant::now();
        copy_file_cancellable(CopyRequest {
            local: &mut local,
            remote: &mut remote,
            remote_path: request.remote_path,
            cancelled: request.cancelled,
        })?;
        let copy_elapsed = copy_started.elapsed().as_millis();
        self.output.line(format!(
            "[sync] upload detail remote={} ensure_ms={} open_ms={} create_ms={} copy_ms={}",
            request.remote_path.display(),
            ensure_elapsed,
            open_elapsed,
            create_elapsed,
            copy_elapsed
        ));
        Ok(())
    }

    fn create_remote_file(&mut self, remote_path: &Path) -> Result<ssh2::File> {
        match self.sftp()?.create(remote_path) {
            Ok(file) => Ok(file),
            Err(first_error) => {
                self.invalidate_parent_dir(remote_path);
                self.ensure_parent_dir(remote_path)?;
                self.sftp()?.create(remote_path).map_err(|source| {
                    err_with_source(error_info::SYNC_SFTP_FAILED, source)
                        .with_path(remote_path.display())
                        .with_hint(format!(
                            "retry after parent dir refresh; first error: {first_error}"
                        ))
                })
            }
        }
    }

    pub(crate) fn remove(&mut self, remote_path: &Path) -> Result<()> {
        let command = format!(
            "rm -rf -- {}",
            shell_quote(&remote_path.display().to_string())
        );
        let mut channel = self
            .session
            .channel_session()
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?;
        channel
            .exec(&command)
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?;
        let _output = read_channel_stdout(&mut channel);
        channel
            .wait_close()
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?;
        let code = channel.exit_status().map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_command(command.clone())
        })?;
        if code == 0 {
            Ok(())
        } else {
            Err(err(error_info::SYNC_SFTP_FAILED)
                .with_command(command)
                .with_exit_code(Some(code)))
        }
    }

    pub(crate) fn exec_checked(&mut self, command: &str, check: RemoteCheck) -> Result<()> {
        let mut channel = self.session.channel_session().map_err(|source| {
            check
                .error_with_source(source)
                .with_command(command.to_owned())
        })?;
        channel.exec(command).map_err(|source| {
            check
                .error_with_source(source)
                .with_command(command.to_owned())
        })?;
        let stdout = read_channel_stdout(&mut channel);
        let stderr = read_channel_stderr(&mut channel);
        channel.wait_close().map_err(|source| {
            check
                .error_with_source(source)
                .with_command(command.to_owned())
        })?;
        let code = channel.exit_status().map_err(|source| {
            check
                .error_with_source(source)
                .with_command(command.to_owned())
        })?;
        if code == 0 {
            Ok(())
        } else {
            let mut error = err(check.info)
                .with_command(command.to_owned())
                .with_exit_code(Some(code));
            if !stderr.is_empty() {
                error = error.with_hint(stderr);
            } else if !stdout.is_empty() {
                error = error.with_hint(stdout);
            } else if let Some(hint) = check.hint {
                error = error.with_hint(hint);
            }
            Err(error)
        }
    }

    fn sftp(&mut self) -> Result<&mut Sftp> {
        if self.sftp.is_none() {
            self.sftp = Some(
                self.session
                    .sftp()
                    .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?,
            );
        }
        self.sftp
            .as_mut()
            .ok_or_else(|| err(error_info::INTERNAL_UNEXPECTED))
    }

    fn ensure_parent_dir(&mut self, remote_path: &Path) -> Result<()> {
        let Some(parent) = remote_path.parent() else {
            return Ok(());
        };
        self.ensure_dir(parent)
    }

    fn invalidate_parent_dir(&mut self, remote_path: &Path) {
        if let Some(parent) = remote_path.parent() {
            self.ensured_dirs
                .retain(|dir| dir != parent && !dir.starts_with(parent));
        }
    }

    fn ensure_dir(&mut self, dir: &Path) -> Result<()> {
        if self.ensured_dirs.contains(dir) {
            return Ok(());
        }
        let mut current = PathBuf::new();
        for component in dir.components() {
            current.push(component.as_os_str());
            let path = current.as_path();
            if path.as_os_str().is_empty() || self.ensured_dirs.contains(path) {
                continue;
            }
            match self.sftp()?.mkdir(path, 0o755) {
                Ok(()) => {
                    self.ensured_dirs.insert(path.to_path_buf());
                }
                Err(_) if self.sftp()?.stat(path).is_ok() => {
                    self.ensured_dirs.insert(path.to_path_buf());
                }
                Err(source) => {
                    return Err(err_with_source(error_info::SYNC_SFTP_FAILED, source)
                        .with_path(path.display()));
                }
            }
        }
        self.ensured_dirs.insert(dir.to_path_buf());
        Ok(())
    }
}

struct CopyRequest<'a> {
    local: &'a mut FsFile,
    remote: &'a mut ssh2::File,
    remote_path: &'a Path,
    cancelled: Option<&'a Arc<AtomicBool>>,
}

pub(crate) struct RemoteCheck {
    info: ErrorInfo,
    hint: Option<&'static str>,
}

impl RemoteCheck {
    pub(crate) fn new(info: ErrorInfo) -> Self {
        Self { info, hint: None }
    }

    pub(crate) fn with_hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(hint);
        self
    }

    fn error_with_source(
        &self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> crate::error::RdevError {
        let mut error = err_with_source(self.info, source);
        if let Some(hint) = self.hint {
            error = error.with_hint(hint);
        }
        error
    }
}

pub(crate) fn sh_c(script: &str) -> String {
    format!("sh -c {}", shell_quote(&with_remote_path(script)))
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn read_channel_stderr(channel: &mut ssh2::Channel) -> String {
    let mut stderr = String::new();
    let _read_result = channel.stderr().read_to_string(&mut stderr);
    stderr.trim().to_owned()
}

fn read_channel_stdout(channel: &mut ssh2::Channel) -> String {
    let mut stdout = String::new();
    let _read_result = channel.read_to_string(&mut stdout);
    stdout.trim().to_owned()
}

fn copy_file_cancellable(request: CopyRequest<'_>) -> Result<()> {
    let mut buffer = vec![0_u8; UPLOAD_CHUNK_SIZE];
    loop {
        if is_cancelled(request.cancelled) {
            return Err(err(error_info::SYNC_CANCELLED).with_path(request.remote_path.display()));
        }
        let read = request.local.read(&mut buffer).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source)
                .with_path(request.remote_path.display())
        })?;
        if read == 0 {
            return Ok(());
        }
        request
            .remote
            .write_all(&buffer[..read])
            .map_err(|source| {
                err_with_source(error_info::SYNC_SFTP_FAILED, source)
                    .with_path(request.remote_path.display())
            })?;
    }
}

fn is_cancelled(cancelled: Option<&Arc<AtomicBool>>) -> bool {
    cancelled.is_some_and(|flag| flag.load(Ordering::SeqCst))
}

fn with_remote_path(command: &str) -> String {
    format!("PATH={}:$PATH; {}", shell_quote(REMOTE_BASE_PATH), command)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshEndpoint {
    user: String,
    host: String,
    port: u16,
}

impl SshEndpoint {
    fn parse(remote: &str, port: u16) -> Self {
        let (user, host) = match remote.split_once('@') {
            Some((user, host)) => (user.to_owned(), host.to_owned()),
            None => (default_user(), remote.to_owned()),
        };
        Self { user, host, port }
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn authenticate(session: &Session, user: &str, config: &AppConfig) -> Result<()> {
    let mut attempts = Vec::new();
    match session.userauth_agent(user) {
        Ok(()) if session.authenticated() => return Ok(()),
        Ok(()) => attempts.push("ssh-agent: authenticated=false".to_owned()),
        Err(error) => attempts.push(format!("ssh-agent: {error}")),
    }

    for key in identity_files(config) {
        if !key.exists() {
            attempts.push(format!("key {}: not found", key.display()));
            continue;
        }
        let passphrase = passphrase(config);
        match session.userauth_pubkey_file(user, None, &key, passphrase.as_deref()) {
            Ok(()) if session.authenticated() => return Ok(()),
            Ok(()) => attempts.push(format!("key {}: authenticated=false", key.display())),
            Err(error) => attempts.push(format!("key {}: {error}", key.display())),
        }
    }

    Err(err(error_info::REMOTE_SSH_CONNECT_FAILED)
        .with_remote(config.remote.host.clone())
        .with_hint(format!(
            "内部 SSH 认证失败。请确认 Windows ssh-agent 已启动且 ssh-add 已加载 key。已尝试：{}",
            attempts.join("; ")
        )))
}

fn identity_files(config: &AppConfig) -> Vec<PathBuf> {
    let mut keys = Vec::new();
    let mut seen = BTreeSet::new();
    if !config.remote.identity_file.is_empty() {
        push_key(
            &mut keys,
            &mut seen,
            PathBuf::from(&config.remote.identity_file),
        );
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let ssh = PathBuf::from(home).join(".ssh");
        push_key(&mut keys, &mut seen, ssh.join("id_ed25519"));
        push_key(&mut keys, &mut seen, ssh.join("id_rsa"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let ssh = PathBuf::from(home).join(".ssh");
        push_key(&mut keys, &mut seen, ssh.join("id_ed25519"));
        push_key(&mut keys, &mut seen, ssh.join("id_rsa"));
    }
    keys
}

fn push_key(keys: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>, key: PathBuf) {
    if seen.insert(key.clone()) {
        keys.push(key);
    }
}

fn passphrase(config: &AppConfig) -> Option<String> {
    if config.remote.passphrase_env.is_empty() {
        None
    } else {
        std::env::var(&config.remote.passphrase_env).ok()
    }
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::AppConfig;

    use super::{identity_files, SshEndpoint};

    #[test]
    fn parses_user_host_endpoint() {
        let endpoint = SshEndpoint::parse("root@10.0.0.2", 2222);

        assert_eq!(endpoint.user, "root");
        assert_eq!(endpoint.host, "10.0.0.2");
        assert_eq!(endpoint.address(), "10.0.0.2:2222");
    }

    #[test]
    fn configured_identity_file_is_first() {
        let mut config = AppConfig::template("root@example.com", 22, "/rdev/project");
        config.remote.identity_file = "C:\\Users\\me\\.ssh\\id_ed25519".to_owned();

        let keys = identity_files(&config);

        assert_eq!(
            keys.first(),
            Some(&PathBuf::from("C:\\Users\\me\\.ssh\\id_ed25519"))
        );
    }
}
