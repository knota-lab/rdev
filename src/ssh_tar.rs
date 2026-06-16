use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::{is_sync_excluded, RemotePath};
use crate::ssh::{read_channel_stderr, sh_c, shell_quote, RemoteCheck, SshClient};
use crate::sync::SyncRequest;

pub(crate) struct TarUpload<'a> {
    pub(crate) config: &'a AppConfig,
    pub(crate) request: &'a SyncRequest,
    pub(crate) remote_root: &'a RemotePath,
}

pub(crate) fn upload_tar(client: &mut SshClient, upload: TarUpload<'_>) -> Result<()> {
    if upload.request.is_cancelled() {
        return Err(err(error_info::SYNC_CANCELLED));
    }
    let remote_root = shell_quote(upload.remote_root.as_str());
    client.exec_checked(
        &sh_c(&format!("mkdir -p {remote_root}")),
        RemoteCheck::new(error_info::SYNC_SSH_TAR_FAILED)
            .with_hint("failed to create remote directory"),
    )?;
    client.exec_checked(
        &sh_c(&format!("test -d {remote_root}")),
        RemoteCheck::new(error_info::SYNC_SSH_TAR_FAILED)
            .with_hint("remote path is not a directory"),
    )?;
    client.exec_checked(
        &sh_c(&format!("test -w {remote_root}")),
        RemoteCheck::new(error_info::SYNC_SSH_TAR_FAILED)
            .with_hint("remote directory is not writable"),
    )?;
    let command = sh_c(&format!(
        "tar -xf - -C {}",
        shell_quote(upload.remote_root.as_str())
    ));
    let mut channel = client
        .session_mut()
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

#[derive(Debug)]
struct TarProgress {
    started: Instant,
    last_printed: Instant,
    files: u64,
    dirs: u64,
    bytes: u64,
}

impl TarProgress {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_printed: now,
            files: 0,
            dirs: 0,
            bytes: 0,
        }
    }

    fn record_file(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.print_if_due();
    }

    fn record_dir(&mut self) {
        self.dirs += 1;
        self.print_if_due();
    }

    fn print_if_due(&mut self) {
        if self.last_printed.elapsed() >= Duration::from_secs(2) {
            self.print("progress");
            self.last_printed = Instant::now();
        }
    }

    fn print(&self, label: &str) {
        println!(
            "[sync] tar {label} files={} dirs={} bytes={} elapsed_ms={}",
            self.files,
            self.dirs,
            self.bytes,
            self.started.elapsed().as_millis()
        );
    }
}

fn append_project_tar<W: Write>(
    archive: &mut tar::Builder<W>,
    upload: TarUpload<'_>,
) -> Result<()> {
    let mut progress = TarProgress::new();
    for watch_dir in &upload.config.sync.watch_dirs {
        let source = local_source(&upload.request.project_root, watch_dir);
        for entry in WalkDir::new(&source)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry.path() == source
                    || !is_sync_excluded(entry.path(), &source, &upload.config.sync.exclude)
            })
        {
            if upload.request.is_cancelled() {
                return Err(err(error_info::SYNC_CANCELLED));
            }
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
                progress.record_dir();
            } else if entry.file_type().is_file() {
                let bytes = entry.metadata().map_or(0, |metadata| metadata.len());
                archive
                    .append_path_with_name(path, archive_name)
                    .map_err(|source| err_with_source(error_info::SYNC_SSH_TAR_FAILED, source))?;
                progress.record_file(bytes);
            }
        }
    }
    progress.print("done");
    Ok(())
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
