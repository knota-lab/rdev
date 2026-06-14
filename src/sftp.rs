use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::config::AppConfig;
use crate::error::{err_with_source, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};
use crate::sync::{SyncDeltaRequest, SyncReport};

pub struct SftpDeltaBackend<'a, R> {
    config: &'a AppConfig,
    runner: &'a R,
    control_path: PathBuf,
    control_enabled: Cell<bool>,
    session: RefCell<Option<PersistentSftpSession>>,
}

impl<'a, R> SftpDeltaBackend<'a, R>
where
    R: ProcessRunner,
{
    pub fn new(config: &'a AppConfig, runner: &'a R) -> Self {
        Self {
            config,
            runner,
            control_path: control_path(config),
            control_enabled: Cell::new(false),
            session: RefCell::new(None),
        }
    }

    pub fn warm_up(&self) -> Result<()> {
        self.ensure_session()
    }

    pub fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        if !request.uploads.is_empty() {
            self.upload_batch(UploadBatch {
                project_root: &request.project_root,
                remote_root: &remote_root,
                uploads: &request.uploads,
            })?;
        }
        if !request.deletes.is_empty() {
            self.delete_batch(&remote_root, &request.deletes)?;
        }
        Ok(SyncReport::completed_sftp(request.uploads, request.deletes))
    }

    pub fn shutdown(&self) {
        if let Some(mut session) = self.session.replace(None) {
            session.shutdown();
        }
    }

    fn upload_batch(&self, upload: UploadBatch<'_>) -> Result<()> {
        let batch = build_upload_batch(upload.project_root, upload.remote_root, upload.uploads);
        self.write_sftp_batch(&batch)
    }

    fn ensure_session(&self) -> Result<()> {
        if self.session.borrow().is_none() {
            self.session
                .replace(Some(PersistentSftpSession::connect(self.config)?));
        }
        Ok(())
    }

    fn write_sftp_batch(&self, batch: &str) -> Result<()> {
        self.ensure_session()?;
        let mut session_ref = self.session.borrow_mut();
        let Some(session) = session_ref.as_mut() else {
            return Err(crate::error::err(error_info::INTERNAL_UNEXPECTED));
        };
        if session.has_exited()? {
            drop(session_ref);
            self.session
                .replace(Some(PersistentSftpSession::connect(self.config)?));
            let mut session_ref = self.session.borrow_mut();
            let Some(session) = session_ref.as_mut() else {
                return Err(crate::error::err(error_info::INTERNAL_UNEXPECTED));
            };
            return session.write_batch(batch);
        }
        session.write_batch(batch)
    }

    fn delete_batch(&self, remote_root: &RemotePath, deletes: &[PathBuf]) -> Result<()> {
        let command = build_delete_command(DeleteBatch {
            config: self.config,
            control_path: if self.control_enabled.get() {
                Some(self.control_path.as_path())
            } else {
                None
            },
            remote_root,
            deletes,
        });
        let display = command.display();
        let output = self.runner.output(command).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_command(display.clone())
        })?;
        check_output(output, display)
    }
}

struct PersistentSftpSession {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl PersistentSftpSession {
    fn connect(config: &AppConfig) -> Result<Self> {
        let mut command = StdCommand::new("sftp");
        command
            .arg("-q")
            .arg("-b")
            .arg("-")
            .arg("-P")
            .arg(config.remote.port.to_string())
            .arg(config.remote.host.clone())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_sftp_process(&mut command);
        let mut child = command
            .spawn()
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => return Err(crate::error::err(error_info::SYNC_SFTP_FAILED)),
        };
        Ok(Self {
            child,
            stdin: Some(stdin),
        })
    }

    fn write_batch(&mut self, batch: &str) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(crate::error::err(error_info::SYNC_SFTP_FAILED));
        };
        stdin
            .write_all(batch.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))
    }

    fn has_exited(&mut self) -> Result<bool> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|source| err_with_source(error_info::SYNC_SFTP_FAILED, source))
    }

    fn shutdown(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _write_result = stdin.write_all(b"bye\n");
            let _flush_result = stdin.flush();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                Ok(None) | Err(_) => {
                    let _kill_result = self.child.kill();
                    let _wait_result = self.child.wait();
                    return;
                }
            }
        }
    }
}

#[cfg(windows)]
fn configure_sftp_process(command: &mut StdCommand) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn configure_sftp_process(_command: &mut StdCommand) {}

struct UploadBatch<'a> {
    project_root: &'a Path,
    remote_root: &'a RemotePath,
    uploads: &'a [PathBuf],
}

fn build_upload_batch(
    project_root: &Path,
    remote_root: &RemotePath,
    uploads: &[PathBuf],
) -> String {
    let mut lines = Vec::new();
    for relative in uploads {
        let local_path = project_root.join(relative);
        let remote_path = remote_path(remote_root, relative);
        if local_path.is_dir() {
            lines.extend(mkdir_lines(Path::new(&remote_path)));
        } else {
            if let Some(parent) = Path::new(&remote_path).parent() {
                lines.extend(mkdir_lines(parent));
            }
            lines.push(format!(
                "put {} {}",
                batch_quote(&local_path.display().to_string()),
                batch_quote(&remote_path)
            ));
        }
    }
    lines.join("\n")
}

fn mkdir_lines(dir: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = PathBuf::new();
    for component in dir.components() {
        current.push(component.as_os_str());
        let path = current.display().to_string().replace('\\', "/");
        if path == "/" || path.is_empty() {
            continue;
        }
        lines.push(format!("-mkdir {}", batch_quote(&path)));
    }
    lines
}

struct DeleteBatch<'a> {
    config: &'a AppConfig,
    control_path: Option<&'a Path>,
    remote_root: &'a RemotePath,
    deletes: &'a [PathBuf],
}

fn build_delete_command(delete: DeleteBatch<'_>) -> ProcessCommand {
    let delete_commands = delete
        .deletes
        .iter()
        .map(|path| {
            let path = remote_path(delete.remote_root, path);
            format!("rm -rf -- {}", shell_quote(&path))
        })
        .collect::<Vec<_>>()
        .join(" && ");
    let remote_shell = format!("sh -lc {}", shell_quote(&delete_commands));
    let mut command = ProcessCommand::new("ssh");
    if let Some(control_path) = delete.control_path {
        command = command.args(control_options(control_path));
    }
    command
        .arg("-p")
        .arg(delete.config.remote.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(delete.config.remote.host.clone())
        .arg(remote_shell)
}

fn control_options(control_path: &Path) -> Vec<String> {
    vec![
        "-o".to_owned(),
        "ControlMaster=auto".to_owned(),
        "-o".to_owned(),
        "ControlPersist=10m".to_owned(),
        "-o".to_owned(),
        format!("ControlPath={}", control_path.display()),
    ]
}

fn control_path(config: &AppConfig) -> PathBuf {
    let raw = format!(
        "rdev-{}-{}",
        config.remote.host.replace(['@', ':', '\\', '/', '.'], "_"),
        config.remote.port
    );
    std::env::temp_dir().join(raw)
}

fn check_output(output: ProcessOutput, command: String) -> Result<()> {
    if output.code == Some(0) {
        Ok(())
    } else {
        let mut error = crate::error::err(error_info::SYNC_SFTP_FAILED)
            .with_command(command)
            .with_exit_code(output.code);
        if !output.stderr.is_empty() {
            error = error.with_hint(output.stderr);
        }
        Err(error)
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

fn batch_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::AppConfig;
    use crate::path::RemotePath;

    use super::{build_delete_command, build_upload_batch, control_path, remote_path, DeleteBatch};

    #[test]
    fn builds_upload_batch_with_parent_dirs() {
        let root = parse_root();
        let batch = build_upload_batch(
            &PathBuf::from("J:\\project"),
            &root,
            &[PathBuf::from("src\\main.rs")],
        );

        assert!(batch.contains("-mkdir \"/rdev\""));
        assert!(batch.contains("-mkdir \"/rdev/project/src\""));
        assert!(batch.contains("put \"J:\\project\\src\\main.rs\" \"/rdev/project/src/main.rs\""));
    }

    #[test]
    fn builds_delete_command() {
        let root = parse_root();
        let config = AppConfig::template("root@example.com", 22, "/rdev/project");

        let deletes = [PathBuf::from("src\\old.rs")];
        let command = build_delete_command(DeleteBatch {
            config: &config,
            control_path: Some(&control_path(&config)),
            remote_root: &root,
            deletes: &deletes,
        });

        let display = command.display();
        assert!(display.contains("ssh"));
        assert!(display.contains("ControlMaster=auto"));
        assert!(display.contains("rm -rf --"));
        assert!(display.contains("/rdev/project/src/old.rs"));
    }

    #[test]
    fn builds_remote_path_with_forward_slashes() {
        let root = parse_root();

        assert_eq!(
            remote_path(&root, &PathBuf::from("src\\main.rs")),
            "/rdev/project/src/main.rs"
        );
    }

    fn parse_root() -> RemotePath {
        match RemotePath::parse("/rdev/project") {
            Ok(root) => root,
            Err(error) => panic!("remote path should parse: {error}"),
        }
    }
}
