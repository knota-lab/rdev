use std::path::{Path, PathBuf};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    mode: DetectedRsync,
    synced_roots: Vec<String>,
    dry_run: bool,
}

impl SyncReport {
    pub fn format_text(&self) -> String {
        let mode = match self.mode {
            DetectedRsync::Native => "native",
            DetectedRsync::Wsl => "wsl",
        };
        let action = if self.dry_run { "dry-run" } else { "sync" };
        format!(
            "[sync] {action} completed via {mode}: {}",
            self.synced_roots.join(", ")
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedRsync {
    Native,
    Wsl,
}

pub struct RsyncSyncBackend<'a, R> {
    config: &'a AppConfig,
    runner: &'a R,
}

impl<'a, R> RsyncSyncBackend<'a, R>
where
    R: ProcessRunner,
{
    pub fn new(config: &'a AppConfig, runner: &'a R) -> Self {
        Self { config, runner }
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
            mode,
            synced_roots,
            dry_run: request.dry_run,
        })
    }

    fn detect_rsync(&self) -> Result<DetectedRsync> {
        match self.config.sync.rsync_mode {
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
        }
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
            Err(err(error_info::TOOL_RSYNC_NOT_FOUND).with_command(display))
        }
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
    args.extend(
        config
            .sync
            .exclude
            .iter()
            .map(|exclude| format!("--exclude={exclude}")),
    );
    args
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
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert_eq!(commands.len(), 2);
        assert!(commands[1].contains("wsl bash -lc"));
        assert!(commands[1].contains("--dry-run"));
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
        });

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert!(commands[1].contains("'/mnt/j/RustWorkspace/project/'"));
        assert!(commands[1].contains("'root@example.com:/root/project/'"));
        assert!(!commands[1].contains("/./"));
    }
}
