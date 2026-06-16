use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::{AppConfig, RsyncMode};
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};

#[derive(Debug, Clone)]
pub struct SyncRequest {
    pub dry_run: bool,
    pub delete: bool,
    pub project_root: PathBuf,
    pub cancelled: Option<Arc<AtomicBool>>,
}

impl SyncRequest {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
    }
}

#[derive(Debug, Clone)]
pub struct SyncDeltaRequest {
    pub project_root: PathBuf,
    pub uploads: Vec<PathBuf>,
    pub deletes: Vec<PathBuf>,
}

pub trait SyncBackend {
    fn warm_up(&self) -> Result<()> {
        Ok(())
    }

    fn sync_full(&self, request: SyncRequest) -> Result<SyncReport>;

    fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    mode: SyncMode,
    synced_roots: Vec<String>,
    dry_run: bool,
}

impl SyncReport {
    pub fn format_text(&self) -> String {
        let mode = match self.mode {
            SyncMode::NativeRsync => "native",
            SyncMode::WslRsync => "wsl",
            SyncMode::Sftp => "sftp",
            SyncMode::SshTar => "ssh-tar",
        };
        let action = if self.dry_run { "dry-run" } else { "sync" };
        format!(
            "[sync] {action} completed via {mode}: {}",
            self.synced_roots.join(", ")
        )
    }

    pub fn completed_sftp(uploads: Vec<PathBuf>, deletes: Vec<PathBuf>) -> Self {
        let synced_roots = uploads
            .into_iter()
            .chain(deletes)
            .map(|path| path.display().to_string())
            .collect();
        Self {
            mode: SyncMode::Sftp,
            synced_roots,
            dry_run: false,
        }
    }

    pub fn completed_ssh_tar(watch_dirs: Vec<PathBuf>, dry_run: bool) -> Self {
        let synced_roots = watch_dirs
            .into_iter()
            .map(|path| path.display().to_string())
            .collect();
        Self {
            mode: SyncMode::SshTar,
            synced_roots,
            dry_run,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncMode {
    NativeRsync,
    WslRsync,
    Sftp,
    SshTar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedRsync {
    Native,
    Wsl,
}

pub struct RsyncSyncBackend<'a, R> {
    config: &'a AppConfig,
    runner: &'a R,
    detected_rsync: Cell<Option<DetectedRsync>>,
}

impl<'a, R> RsyncSyncBackend<'a, R>
where
    R: ProcessRunner,
{
    pub fn new(config: &'a AppConfig, runner: &'a R) -> Self {
        Self {
            config,
            runner,
            detected_rsync: Cell::new(None),
        }
    }

    pub fn sync_full(&self, request: SyncRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        let mode = self.detect_rsync()?;
        let mut synced_roots = Vec::new();
        for watch_dir in &self.config.sync.watch_dirs {
            let build = RsyncCommandBuild {
                config: self.config,
                request: &request,
                remote_root: &remote_root,
                watch_dir,
            };
            let command = build_rsync_command(build, mode);
            let display = command.display();
            let output = self.runner.output(command).map_err(|source| {
                err_with_source(error_info::SYNC_RSYNC_FAILED, source).with_command(display.clone())
            })?;
            if output.code != Some(0) {
                return Err(rsync_failed(output, display));
            }
            synced_roots.push(watch_dir.display().to_string());
        }
        Ok(SyncReport {
            mode: mode.into(),
            synced_roots,
            dry_run: request.dry_run,
        })
    }

    pub fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        let mode = self.detect_rsync()?;
        let mut synced_roots = Vec::new();

        for path in &request.uploads {
            let build = DeltaCommandBuild {
                config: self.config,
                project_root: &request.project_root,
                remote_root: &remote_root,
                relative_path: path,
            };
            let command = build_delta_upload_command(build, mode);
            let display = command.display();
            let output = self.runner.output(command).map_err(|source| {
                err_with_source(error_info::SYNC_RSYNC_FAILED, source).with_command(display.clone())
            })?;
            if output.code != Some(0) {
                return Err(rsync_failed(output, display));
            }
            synced_roots.push(path.display().to_string());
        }

        for path in &request.deletes {
            let command = build_remote_delete_command(self.config, &remote_root, path);
            let display = command.display();
            let output = self.runner.output(command).map_err(|source| {
                err_with_source(error_info::SYNC_RSYNC_FAILED, source).with_command(display.clone())
            })?;
            if output.code != Some(0) {
                return Err(rsync_failed(output, display));
            }
            synced_roots.push(path.display().to_string());
        }

        Ok(SyncReport {
            mode: mode.into(),
            synced_roots,
            dry_run: false,
        })
    }

    fn detect_rsync(&self) -> Result<DetectedRsync> {
        if let Some(mode) = self.detected_rsync.get() {
            return Ok(mode);
        }
        let mode = match self.config.sync.rsync_mode {
            RsyncMode::Native => {
                self.check_native_rsync()?;
                Ok(DetectedRsync::Native)
            }
            RsyncMode::Wsl => {
                self.check_wsl_rsync()?;
                Ok(DetectedRsync::Wsl)
            }
            RsyncMode::Auto => match self.check_native_rsync() {
                Ok(()) => Ok(DetectedRsync::Native),
                Err(_) => {
                    self.check_wsl_rsync()?;
                    Ok(DetectedRsync::Wsl)
                }
            },
        }?;
        self.detected_rsync.set(Some(mode));
        Ok(mode)
    }

    fn check_native_rsync(&self) -> Result<()> {
        let command = ProcessCommand::new("rsync").arg("--version");
        let display = command.display();
        let output = self.runner.output(command).map_err(|source| {
            err_with_source(error_info::TOOL_RSYNC_NOT_FOUND, source).with_command(display.clone())
        })?;
        if output.code == Some(0) {
            Ok(())
        } else {
            Err(err(error_info::TOOL_RSYNC_NOT_FOUND).with_command(display))
        }
    }

    fn check_wsl_rsync(&self) -> Result<()> {
        let command = ProcessCommand::new("wsl")
            .arg("bash")
            .arg("-lc")
            .arg("rsync --version");
        let display = command.display();
        let output = self.runner.output(command).map_err(|source| {
            err_with_source(error_info::TOOL_RSYNC_NOT_FOUND, source).with_command(display.clone())
        })?;
        if output.code == Some(0) {
            Ok(())
        } else {
            Err(tool_rsync_error(output, display))
        }
    }
}

impl<R> SyncBackend for RsyncSyncBackend<'_, R>
where
    R: ProcessRunner,
{
    fn sync_full(&self, request: SyncRequest) -> Result<SyncReport> {
        RsyncSyncBackend::sync_full(self, request)
    }

    fn sync_delta(&self, request: SyncDeltaRequest) -> Result<SyncReport> {
        RsyncSyncBackend::sync_delta(self, request)
    }
}

impl From<DetectedRsync> for SyncMode {
    fn from(value: DetectedRsync) -> Self {
        match value {
            DetectedRsync::Native => Self::NativeRsync,
            DetectedRsync::Wsl => Self::WslRsync,
        }
    }
}

struct DeltaCommandBuild<'a> {
    config: &'a AppConfig,
    project_root: &'a Path,
    remote_root: &'a RemotePath,
    relative_path: &'a Path,
}

fn build_delta_upload_command(build: DeltaCommandBuild<'_>, mode: DetectedRsync) -> ProcessCommand {
    match mode {
        DetectedRsync::Native => ProcessCommand::new("rsync")
            .args(delta_rsync_args(build.config))
            .arg(path_to_forward_slashes(build.relative_path))
            .arg(format!(
                "{}:{}/",
                build.config.remote.host,
                build.remote_root.as_str()
            ))
            .current_dir(build.project_root.to_path_buf()),
        DetectedRsync::Wsl => {
            let root = if !build.config.sync.rsync_local_path.is_empty() {
                build
                    .config
                    .sync
                    .rsync_local_path
                    .trim_end_matches('/')
                    .to_owned()
            } else {
                windows_path_to_wsl(build.project_root)
            };
            let rel = path_to_forward_slashes(build.relative_path);
            let remote = format!(
                "{}:{}/",
                build.config.remote.host,
                build.remote_root.as_str()
            );
            let mut parts = vec![
                "cd".to_owned(),
                shell_quote(&root),
                "&&".to_owned(),
                "rsync".to_owned(),
            ];
            parts.extend(delta_rsync_args(build.config));
            parts.push(shell_quote(&rel));
            parts.push(shell_quote(&remote));
            ProcessCommand::new("wsl")
                .arg("bash")
                .arg("-lc")
                .arg(parts.join(" "))
        }
    }
}

fn build_remote_delete_command(
    config: &AppConfig,
    remote_root: &RemotePath,
    relative_path: &Path,
) -> ProcessCommand {
    let remote_path = format!(
        "{}/{}",
        remote_root.as_str().trim_end_matches('/'),
        path_to_forward_slashes(relative_path)
    );
    let remote_shell = format!(
        "sh -lc {}",
        shell_quote(&format!("rm -rf -- {}", shell_quote(&remote_path)))
    );
    ProcessCommand::new("ssh")
        .arg("-p")
        .arg(config.remote.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(config.remote.host.clone())
        .arg(remote_shell)
}

fn delta_rsync_args(config: &AppConfig) -> Vec<String> {
    let mut args = vec!["-azR".to_owned()];
    args.extend(rsync_filter_args(&config.sync.exclude));
    args
}

fn tool_rsync_error(output: ProcessOutput, command: String) -> crate::error::RdevError {
    let mut error = err(error_info::TOOL_RSYNC_NOT_FOUND)
        .with_command(command)
        .with_exit_code(output.code);
    let hint = first_non_empty(&output.stderr, &output.stdout)
        .unwrap_or("请确认 WSL 默认发行版可启动，并且其中 rsync 在 PATH 中");
    error = error.with_hint(hint);
    error
}

fn first_non_empty<'a>(first: &'a str, second: &'a str) -> Option<&'a str> {
    if !first.is_empty() {
        Some(first)
    } else if !second.is_empty() {
        Some(second)
    } else {
        None
    }
}

struct RsyncCommandBuild<'a> {
    config: &'a AppConfig,
    request: &'a SyncRequest,
    remote_root: &'a RemotePath,
    watch_dir: &'a Path,
}

fn build_rsync_command(build: RsyncCommandBuild<'_>, mode: DetectedRsync) -> ProcessCommand {
    match mode {
        DetectedRsync::Native => build_native_rsync_command(build),
        DetectedRsync::Wsl => build_wsl_rsync_command(build),
    }
}

fn build_native_rsync_command(build: RsyncCommandBuild<'_>) -> ProcessCommand {
    let source = local_source(build.request, build.watch_dir);
    let remote = remote_dest(build.config, build.remote_root, build.watch_dir);
    let mut command =
        ProcessCommand::new("rsync").args(common_rsync_args(build.config, build.request));
    command = command.arg(format!("{}/", source.display())).arg(remote);
    command
}

fn build_wsl_rsync_command(build: RsyncCommandBuild<'_>) -> ProcessCommand {
    let source = wsl_source(build.config, build.request, build.watch_dir);
    let remote = remote_dest(build.config, build.remote_root, build.watch_dir);
    let mut parts = vec!["rsync".to_owned()];
    parts.extend(common_rsync_args(build.config, build.request));
    parts.push(shell_quote(&format!("{source}/")));
    parts.push(shell_quote(&remote));
    ProcessCommand::new("wsl")
        .arg("bash")
        .arg("-lc")
        .arg(parts.join(" "))
}

fn common_rsync_args(config: &AppConfig, request: &SyncRequest) -> Vec<String> {
    let mut args = vec!["-az".to_owned()];
    if request.dry_run {
        args.push("--dry-run".to_owned());
    }
    if request.delete {
        args.push("--delete".to_owned());
    }
    args.extend(rsync_filter_args(&config.sync.exclude));
    args
}

fn rsync_filter_args(excludes: &[String]) -> Vec<String> {
    excludes
        .iter()
        .rev()
        .filter_map(|rule| {
            let trimmed = rule.trim();
            if trimmed.is_empty() {
                None
            } else if let Some(include) = trimmed.strip_prefix('!') {
                Some(format!("--include={}", rsync_path_pattern(include.trim())))
            } else {
                Some(format!("--exclude={}", rsync_path_pattern(trimmed)))
            }
        })
        .collect()
}

fn rsync_path_pattern(pattern: &str) -> String {
    let normalized = pattern.trim().trim_matches('/').replace('\\', "/");
    if normalized.is_empty() || !normalized.contains('/') {
        normalized
    } else {
        format!("**/{normalized}/***")
    }
}

fn remote_dest(config: &AppConfig, remote_root: &RemotePath, watch_dir: &Path) -> String {
    let suffix = path_to_forward_slashes(watch_dir);
    if suffix.is_empty() {
        format!("{}:{}/", config.remote.host, remote_root.as_str())
    } else {
        format!(
            "{}:{}/{}/",
            config.remote.host,
            remote_root.as_str().trim_end_matches('/'),
            suffix
        )
    }
}

fn wsl_source(config: &AppConfig, request: &SyncRequest, watch_dir: &Path) -> String {
    if !config.sync.rsync_local_path.is_empty() {
        return join_wsl_path(&config.sync.rsync_local_path, watch_dir);
    }
    let source = local_source(request, watch_dir);
    windows_path_to_wsl(&source)
}

fn local_source(request: &SyncRequest, watch_dir: &Path) -> PathBuf {
    if is_current_dir(watch_dir) {
        request.project_root.clone()
    } else {
        request.project_root.join(watch_dir)
    }
}

fn windows_path_to_wsl(path: &Path) -> String {
    let raw = path.display().to_string().replace('\\', "/");
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = raw[2..].trim_start_matches('/');
        format!("/mnt/{drive}/{rest}")
    } else {
        raw
    }
}

fn join_wsl_path(root: &str, path: &Path) -> String {
    let suffix = path_to_forward_slashes(path);
    if suffix.is_empty() {
        root.trim_end_matches('/').to_owned()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), suffix)
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

fn is_current_dir(path: &Path) -> bool {
    path.components().all(|component| {
        let item = component.as_os_str().to_string_lossy();
        item == "."
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn rsync_failed(output: ProcessOutput, command: String) -> crate::error::RdevError {
    let mut error = err(error_info::SYNC_RSYNC_FAILED)
        .with_command(command)
        .with_exit_code(output.code);
    if !output.stderr.is_empty() {
        error = error.with_hint(output.stderr);
    }
    error
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use crate::config::{AppConfig, RsyncMode};
    use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};

    use super::{RsyncSyncBackend, SyncRequest};

    #[derive(Debug)]
    struct FakeRunner {
        outputs: RefCell<VecDeque<ProcessOutput>>,
        commands: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn success(count: usize) -> Self {
            let outputs = (0..count)
                .map(|_| ProcessOutput {
                    code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                })
                .collect();
            Self {
                outputs: RefCell::new(outputs),
                commands: RefCell::new(Vec::new()),
            }
        }

        fn from_codes(codes: impl IntoIterator<Item = Option<i32>>) -> Self {
            let outputs = codes
                .into_iter()
                .map(|code| ProcessOutput {
                    code,
                    stdout: String::new(),
                    stderr: "rsync failed".to_owned(),
                })
                .collect();
            Self {
                outputs: RefCell::new(outputs),
                commands: RefCell::new(Vec::new()),
            }
        }
    }

    impl ProcessRunner for FakeRunner {
        fn output(&self, command: ProcessCommand) -> std::io::Result<ProcessOutput> {
            self.commands.borrow_mut().push(command.display());
            match self.outputs.borrow_mut().pop_front() {
                Some(output) => Ok(output),
                None => panic!("fake output missing"),
            }
        }
    }

    #[test]
    fn builds_wsl_rsync_command_for_watch_dir() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.sync.rsync_mode = RsyncMode::Wsl;
        config.sync.watch_dirs = vec![PathBuf::from("backend")];
        let runner = FakeRunner::success(2);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let report = backend.sync_full(SyncRequest {
            dry_run: true,
            delete: true,
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            cancelled: None,
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert_eq!(commands.len(), 2);
        assert!(commands[1].contains("wsl bash -lc"));
        assert!(commands[1].contains("--dry-run"));
        assert!(commands[1].contains("--exclude=data"));
        assert!(
            commands[1].contains("/mnt/j/RustWorkspace/project/backend/"),
            "unexpected command: {}",
            commands[1]
        );
        assert!(commands[1].contains("root@example.com:/root/project/backend/"));
    }

    #[test]
    fn maps_rsync_failure_to_sync_error() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.sync.rsync_mode = RsyncMode::Wsl;
        config.sync.watch_dirs = vec![PathBuf::from("backend")];
        let runner = FakeRunner::from_codes([Some(0), Some(12)]);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let result = backend.sync_full(SyncRequest {
            dry_run: true,
            delete: true,
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            cancelled: None,
        });

        assert!(result.is_err());
        let error = match result {
            Ok(_) => panic!("sync should fail"),
            Err(error) => error,
        };
        assert_eq!(error.info.code, "sync.rsync_failed");
        assert_eq!(error.context.exit_code, Some(12));
        assert!(error.context.command.is_some());
    }

    #[test]
    fn builds_wsl_delta_upload_command() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.sync.rsync_mode = RsyncMode::Wsl;
        let runner = FakeRunner::success(2);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let report = backend.sync_delta(super::SyncDeltaRequest {
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            uploads: vec![PathBuf::from("src/main.rs")],
            deletes: Vec::new(),
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert_eq!(commands.len(), 2);
        assert!(commands[1].contains("rsync -azR"));
        assert!(commands[1].contains("--exclude=data"));
        assert!(commands[1].contains("'src/main.rs'"));
        assert!(commands[1].contains("'root@example.com:/root/project/'"));
    }

    #[test]
    fn maps_negated_exclude_to_rsync_include() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.sync.rsync_mode = RsyncMode::Wsl;
        config.sync.exclude = vec!["data".to_owned(), "!src/data".to_owned()];
        let runner = FakeRunner::success(2);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let report = backend.sync_delta(super::SyncDeltaRequest {
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            uploads: vec![PathBuf::from("knota-fold/src/data/mod.rs")],
            deletes: Vec::new(),
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        let include_index = match commands[1].find("--include=**/src/data/***") {
            Some(index) => index,
            None => panic!("missing include rule: {}", commands[1]),
        };
        let exclude_index = match commands[1].find("--exclude=data") {
            Some(index) => index,
            None => panic!("missing exclude rule: {}", commands[1]),
        };
        assert!(include_index < exclude_index);
    }

    #[test]
    fn caches_detected_rsync_mode() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.sync.rsync_mode = RsyncMode::Wsl;
        let runner = FakeRunner::success(3);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let first = backend.sync_delta(super::SyncDeltaRequest {
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            uploads: vec![PathBuf::from("src/main.rs")],
            deletes: Vec::new(),
        });
        let second = backend.sync_delta(super::SyncDeltaRequest {
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            uploads: vec![PathBuf::from("src/lib.rs")],
            deletes: Vec::new(),
        });

        assert!(first.is_ok());
        assert!(second.is_ok());
        let commands = runner.commands.borrow();
        assert_eq!(commands.len(), 3);
        assert!(commands[0].contains("rsync --version"));
        assert!(commands[1].contains("'src/main.rs'"));
        assert!(commands[2].contains("'src/lib.rs'"));
    }

    #[test]
    fn builds_remote_delete_command() {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let runner = FakeRunner::success(2);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let report = backend.sync_delta(super::SyncDeltaRequest {
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            uploads: Vec::new(),
            deletes: vec![PathBuf::from("src/old.rs")],
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert!(commands[1].contains("ssh"));
        assert!(commands[1].contains("rm -rf --"));
        assert!(commands[1].contains("/root/project/src/old.rs"));
    }

    #[test]
    fn dot_watch_dir_syncs_project_root_without_dot_suffix() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.sync.rsync_mode = RsyncMode::Wsl;
        config.sync.watch_dirs = vec![PathBuf::from(".")];
        let runner = FakeRunner::success(2);
        let backend = RsyncSyncBackend::new(&config, &runner);

        let report = backend.sync_full(SyncRequest {
            dry_run: true,
            delete: true,
            project_root: PathBuf::from("J:\\RustWorkspace\\project"),
            cancelled: None,
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert!(commands[1].contains("'/mnt/j/RustWorkspace/project/'"));
        assert!(commands[1].contains("'root@example.com:/root/project/'"));
        assert!(!commands[1].contains("/./"));
    }
}
