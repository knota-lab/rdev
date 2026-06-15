use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ssh2::{Session, Sftp};
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::error::{err, err_with_source, ErrorInfo, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::sync::{SyncBackend, SyncDeltaRequest, SyncReport, SyncRequest};

const REMOTE_BASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct SftpDeltaBackend<'a> {
    config: &'a AppConfig,
    client: RefCell<Option<SftpClient>>,
}

impl<'a> SftpDeltaBackend<'a> {
    pub fn new(config: &'a AppConfig) -> Self {
        Self {
            config,
            client: RefCell::new(None),
        }
    }

    fn sync_delta_impl(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        println!(
            "[sync] delta start transport=internal-sftp uploads={} deletes={}",
            request.uploads.len(),
            request.deletes.len()
        );
        let started = Instant::now();
        self.with_client(|client| {
            for path in &request.uploads {
                let item_started = Instant::now();
                let local_path = request.project_root.join(path);
                let remote_path = remote_path(&remote_root, path);
                client.upload(&local_path, Path::new(&remote_path))?;
                println!(
                    "[sync] upload ok path={} elapsed_ms={}",
                    path.display(),
                    item_started.elapsed().as_millis()
                );
            }
            for path in &request.deletes {
                let item_started = Instant::now();
                let remote_path = remote_path(&remote_root, path);
                client.remove(Path::new(&remote_path))?;
                println!(
                    "[sync] delete ok path={} elapsed_ms={}",
                    path.display(),
                    item_started.elapsed().as_millis()
                );
            }
            Ok(())
        })?;
        println!(
            "[sync] delta done elapsed_ms={}",
            started.elapsed().as_millis()
        );
        Ok(SyncReport::completed_sftp(request.uploads, request.deletes))
    }

    fn sync_full_impl(&self, request: SyncRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        if request.dry_run {
            println!("[sync] full dry-run transport=ssh-tar");
            return Ok(SyncReport::completed_ssh_tar(
                self.config.sync.watch_dirs.clone(),
                true,
            ));
        }
        let started = Instant::now();
        println!("[sync] full start transport=ssh-tar");
        self.with_client(|client| {
            client.upload_tar(TarUpload {
                config: self.config,
                request: &request,
                remote_root: &remote_root,
            })
        })?;
        println!(
            "[sync] full done transport=ssh-tar elapsed_ms={}",
            started.elapsed().as_millis()
        );
        Ok(SyncReport::completed_ssh_tar(
            self.config.sync.watch_dirs.clone(),
            request.dry_run,
        ))
    }

    fn warm_up_impl(&self) -> Result<()> {
        let started = Instant::now();
        self.with_client(|_| Ok(()))?;
        println!(
            "[sync] internal-sftp connected elapsed_ms={}",
            started.elapsed().as_millis()
        );
        Ok(())
    }

    fn with_client<T>(&self, action: impl FnOnce(&mut SftpClient) -> Result<T>) -> Result<T> {
        if self.client.borrow().is_none() {
            self.client.replace(Some(SftpClient::connect(self.config)?));
        }
        let mut client = self.client.borrow_mut();
        let Some(client) = client.as_mut() else {
            return Err(err(error_info::INTERNAL_UNEXPECTED));
        };
        action(client)
    }
}

impl SyncBackend for SftpDeltaBackend<'_> {
    fn warm_up(&self) -> Result<()> {
        self.warm_up_impl()
    }

    fn sync_full(&self, request: SyncRequest) -> Result<SyncReport> {
        self.sync_full_impl(request)
    }

    fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        self.sync_delta_impl(request)
    }
}

struct SftpClient {
    session: Session,
    sftp: Option<Sftp>,
    ensured_dirs: BTreeSet<PathBuf>,
}

impl SftpClient {
    fn connect(config: &AppConfig) -> Result<Self> {
        let endpoint = SshEndpoint::parse(&config.remote.host, config.remote.port);
        let tcp = TcpStream::connect(endpoint.address()).map_err(|source| {
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
        })
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

    fn upload(&mut self, local_path: &Path, remote_path: &Path) -> Result<()> {
        if local_path.is_dir() {
            return self.ensure_dir(remote_path);
        }
        let ensure_started = Instant::now();
        self.ensure_parent_dir(remote_path)?;
        let ensure_elapsed = ensure_started.elapsed().as_millis();

        let open_started = Instant::now();
        let mut local = File::open(local_path).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(local_path.display())
        })?;
        let open_elapsed = open_started.elapsed().as_millis();

        let create_started = Instant::now();
        let mut remote = self.sftp()?.create(remote_path).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(remote_path.display())
        })?;
        let create_elapsed = create_started.elapsed().as_millis();

        let copy_started = Instant::now();
        io::copy(&mut local, &mut remote).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(remote_path.display())
        })?;
        let copy_elapsed = copy_started.elapsed().as_millis();
        println!(
            "[sync] upload detail remote={} ensure_ms={} open_ms={} create_ms={} copy_ms={}",
            remote_path.display(),
            ensure_elapsed,
            open_elapsed,
            create_elapsed,
            copy_elapsed
        );
        Ok(())
    }

    fn ensure_parent_dir(&mut self, remote_path: &Path) -> Result<()> {
        let Some(parent) = remote_path.parent() else {
            return Ok(());
        };
        self.ensure_dir(parent)
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

    fn remove(&mut self, remote_path: &Path) -> Result<()> {
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

    fn upload_tar(&mut self, upload: TarUpload<'_>) -> Result<()> {
        let remote_root = shell_quote(upload.remote_root.as_str());
        self.exec_checked(
            &sh_c(&format!("mkdir -p {remote_root}")),
            TarCheck::new(error_info::SYNC_SSH_TAR_FAILED)
                .with_hint("failed to create remote directory"),
        )?;
        self.exec_checked(
            &sh_c(&format!("test -d {remote_root}")),
            TarCheck::new(error_info::SYNC_SSH_TAR_FAILED)
                .with_hint("remote path is not a directory"),
        )?;
        self.exec_checked(
            &sh_c(&format!("test -w {remote_root}")),
            TarCheck::new(error_info::SYNC_SSH_TAR_FAILED)
                .with_hint("remote directory is not writable"),
        )?;
        let command = sh_c(&format!(
            "tar -xf - -C {}",
            shell_quote(upload.remote_root.as_str())
        ));
        let mut channel = self
            .session
            .channel_session()
            .map_err(|source| err_with_source(error_info::SYNC_SSH_TAR_FAILED, source))?;
        channel
            .exec(&command)
            .map_err(|source| err_with_source(error_info::SYNC_SSH_TAR_FAILED, source))?;
        let finish_result = {
            let mut archive = tar::Builder::new(&mut channel);
            append_project_tar(&mut archive, upload)?;
            archive.finish()
        };
        finish_result.map_err(|source| tar_channel_error(source, &command, &mut channel))?;
        channel
            .send_eof()
            .map_err(|source| tar_channel_error(source, &command, &mut channel))?;
        channel
            .wait_eof()
            .map_err(|source| tar_channel_error(source, &command, &mut channel))?;
        channel
            .wait_close()
            .map_err(|source| tar_channel_error(source, &command, &mut channel))?;
        let stderr = read_channel_stderr(&mut channel);
        let code = channel.exit_status().map_err(|source| {
            err_with_source(error_info::SYNC_SSH_TAR_FAILED, source).with_command(command.clone())
        })?;
        if code == 0 {
            Ok(())
        } else {
            let mut error = err(error_info::SYNC_SSH_TAR_FAILED)
                .with_command(command)
                .with_exit_code(Some(code));
            if !stderr.is_empty() {
                error = error.with_hint(stderr);
            }
            Err(error)
        }
    }

    fn exec_checked(&mut self, command: &str, check: TarCheck) -> Result<()> {
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
}

struct TarCheck {
    info: ErrorInfo,
    hint: Option<&'static str>,
}

impl TarCheck {
    fn new(info: ErrorInfo) -> Self {
        Self { info, hint: None }
    }

    fn with_hint(mut self, hint: &'static str) -> Self {
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

fn sh_c(script: &str) -> String {
    format!("sh -c {}", shell_quote(&with_remote_path(script)))
}

fn with_remote_path(command: &str) -> String {
    format!("PATH={}:$PATH; {}", shell_quote(REMOTE_BASE_PATH), command)
}

fn tar_channel_error(
    source: impl std::error::Error + Send + Sync + 'static,
    command: &str,
    channel: &mut ssh2::Channel,
) -> crate::error::RdevError {
    let mut error =
        err_with_source(error_info::SYNC_SSH_TAR_FAILED, source).with_command(command.to_owned());
    let stderr = read_channel_stderr(channel);
    if !stderr.is_empty() {
        error = error.with_hint(stderr);
    }
    error
}

fn read_channel_stderr(channel: &mut ssh2::Channel) -> String {
    let mut stderr = String::new();
    let _read_result = channel.stderr().read_to_string(&mut stderr);
    stderr.trim().to_owned()
}

fn read_channel_stdout(channel: &mut ssh2::Channel) -> String {
    let mut stdout = String::new();
    let _read_result = channel.read_to_string(&mut stdout);
    stdout.trim().to_owned()
}

struct TarUpload<'a> {
    config: &'a AppConfig,
    request: &'a SyncRequest,
    remote_root: &'a RemotePath,
}

fn append_project_tar<W: Write>(
    archive: &mut tar::Builder<W>,
    upload: TarUpload<'_>,
) -> Result<()> {
    for watch_dir in &upload.config.sync.watch_dirs {
        let source = local_source(&upload.request.project_root, watch_dir);
        for entry in WalkDir::new(&source)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry.path() == source
                    || !is_excluded(
                        entry.path(),
                        &upload.request.project_root,
                        &upload.config.sync.exclude,
                    )
            })
        {
            let entry =
                entry.map_err(|source| err_with_source(error_info::SYNC_SSH_TAR_FAILED, source))?;
            let path = entry.path();
            if path == source {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&upload.request.project_root) else {
                continue;
            };
            let archive_name = path_to_forward_slashes(relative);
            if entry.file_type().is_dir() {
                archive
                    .append_dir(archive_name, path)
                    .map_err(|source| err_with_source(error_info::SYNC_SSH_TAR_FAILED, source))?;
            } else if entry.file_type().is_file() {
                archive
                    .append_path_with_name(path, archive_name)
                    .map_err(|source| err_with_source(error_info::SYNC_SSH_TAR_FAILED, source))?;
            }
        }
    }
    Ok(())
}

fn local_source(project_root: &Path, watch_dir: &Path) -> PathBuf {
    if watch_dir.components().all(|component| {
        let item = component.as_os_str().to_string_lossy();
        item == "."
    }) {
        project_root.to_path_buf()
    } else {
        project_root.join(watch_dir)
    }
}

fn is_excluded(path: &Path, local_root: &Path, excludes: &[String]) -> bool {
    let Ok(relative) = path.strip_prefix(local_root) else {
        return true;
    };
    relative.components().any(|component| {
        let item = component.as_os_str().to_string_lossy();
        excludes.iter().any(|exclude| exclude == item.as_ref())
    })
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

fn remote_path(remote_root: &RemotePath, relative_path: &Path) -> String {
    format!(
        "{}/{}",
        remote_root.as_str().trim_end_matches('/'),
        path_to_forward_slashes(relative_path)
    )
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| {
            let item = component.as_os_str().to_string_lossy();
            if item == "." {
                None
            } else {
                Some(item.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_owned())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::AppConfig;
    use crate::path::RemotePath;

    use super::{identity_files, remote_path, SshEndpoint};

    #[test]
    fn parses_user_host_endpoint() {
        let endpoint = SshEndpoint::parse("root@10.0.0.2", 2222);

        assert_eq!(endpoint.user, "root");
        assert_eq!(endpoint.host, "10.0.0.2");
        assert_eq!(endpoint.address(), "10.0.0.2:2222");
    }

    #[test]
    fn builds_remote_path_with_forward_slashes() {
        let root = parse_root();

        assert_eq!(
            remote_path(&root, &PathBuf::from("src\\main.rs")),
            "/rdev/project/src/main.rs"
        );
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

    fn parse_root() -> RemotePath {
        match RemotePath::parse("/rdev/project") {
            Ok(root) => root,
            Err(error) => panic!("remote path should parse: {error}"),
        }
    }
}
