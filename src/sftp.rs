use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs::File;
use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

use ssh2::{Session, Sftp};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::sync::{SyncDeltaRequest, SyncReport};

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

    pub fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        self.with_client(|client| {
            for path in &request.uploads {
                let local_path = request.project_root.join(path);
                let remote_path = remote_path(&remote_root, path);
                client.upload(&local_path, Path::new(&remote_path))?;
            }
            for path in &request.deletes {
                let remote_path = remote_path(&remote_root, path);
                client.remove(Path::new(&remote_path))?;
            }
            Ok(())
        })?;
        Ok(SyncReport::completed_sftp(request.uploads, request.deletes))
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

struct SftpClient {
    session: Session,
    sftp: Sftp,
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
        let sftp = session
            .sftp()
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?;
        Ok(Self { session, sftp })
    }

    fn upload(&mut self, local_path: &Path, remote_path: &Path) -> Result<()> {
        if local_path.is_dir() {
            return ensure_dir(&self.sftp, remote_path);
        }
        ensure_parent_dir(&self.sftp, remote_path)?;
        let mut local = File::open(local_path).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(local_path.display())
        })?;
        let mut remote = self.sftp.create(remote_path).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(remote_path.display())
        })?;
        io::copy(&mut local, &mut remote).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(remote_path.display())
        })?;
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

fn ensure_parent_dir(sftp: &Sftp, remote_path: &Path) -> Result<()> {
    let Some(parent) = remote_path.parent() else {
        return Ok(());
    };
    ensure_dir(sftp, parent)
}

fn ensure_dir(sftp: &Sftp, dir: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in dir.components() {
        current.push(component.as_os_str());
        let path = current.as_path();
        if path.as_os_str().is_empty() {
            continue;
        }
        match sftp.mkdir(path, 0o755) {
            Ok(()) => {}
            Err(_) if sftp.stat(path).is_ok() => {}
            Err(source) => {
                return Err(
                    err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(path.display())
                );
            }
        }
    }
    Ok(())
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
