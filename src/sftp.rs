use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::AppConfig;
use crate::error::{err_with_source, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};
use crate::sync::{SyncDeltaRequest, SyncReport};

pub struct SftpDeltaBackend<'a, R> {
    config: &'a AppConfig,
    runner: &'a R,
}

impl<'a, R> SftpDeltaBackend<'a, R>
where
    R: ProcessRunner,
{
    pub fn new(config: &'a AppConfig, runner: &'a R) -> Self {
        Self { config, runner }
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

    fn upload_batch(&self, upload: UploadBatch<'_>) -> Result<()> {
        let batch = build_upload_batch(upload.project_root, upload.remote_root, upload.uploads);
        let batch_path = write_batch_file(&batch)?;
        let result = self.run_sftp_batch(&batch_path);
        let _remove_result = fs::remove_file(&batch_path);
        result
    }

    fn run_sftp_batch(&self, batch_path: &Path) -> Result<()> {
        let command = ProcessCommand::new("sftp")
            .arg("-b")
            .arg(batch_path.display().to_string())
            .arg("-P")
            .arg(self.config.remote.port.to_string())
            .arg(self.config.remote.host.clone());
        let display = command.display();
        let output = self.runner.output(command).map_err(|source| {
            err_with_source(error_info::SYNC_SFTP_FAILED, source).with_command(display.clone())
        })?;
        check_output(output, display)
    }

    fn delete_batch(&self, remote_root: &RemotePath, deletes: &[PathBuf]) -> Result<()> {
        let command = build_delete_command(DeleteBatch {
            config: self.config,
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
    ProcessCommand::new("ssh")
        .arg("-p")
        .arg(delete.config.remote.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(delete.config.remote.host.clone())
        .arg(remote_shell)
}

fn write_batch_file(batch: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("rdev-sftp-{}.batch", batch_id()));
    fs::write(&path, batch).map_err(|source| {
        err_with_source(error_info::SYNC_SFTP_FAILED, source).with_path(path.display())
    })?;
    Ok(path)
}

fn batch_id() -> u128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => 0,
    }
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

    use super::{build_delete_command, build_upload_batch, remote_path, DeleteBatch};

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
            remote_root: &root,
            deletes: &deletes,
        });

        let display = command.display();
        assert!(display.contains("ssh"));
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
