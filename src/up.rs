use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::process::ProcessRunner;
use crate::sftp::SftpDeltaBackend;
use crate::sync::{RsyncSyncBackend, SyncDeltaRequest, SyncRequest};

const RDEV_DIR: &str = ".rdev";
const STOP_FILE: &str = "stop";
const PID_FILE: &str = "up.pid";

#[derive(Debug, Clone)]
pub struct UpRequest {
    pub project_root: PathBuf,
    pub initial_sync: bool,
    pub poll: bool,
}

pub fn run_up(config: &AppConfig, runner: &impl ProcessRunner, request: UpRequest) -> Result<()> {
    let shutdown = install_shutdown_handler()?;
    clear_stop_request(&request.project_root)?;
    write_pid_file(&request.project_root)?;
    let local_root = resolve_local_root(&request.project_root, &config.sync.local_path);
    let rsync_backend = RsyncSyncBackend::new(config, runner);
    let delta_backend = SftpDeltaBackend::new(config);
    if request.initial_sync {
        let report = rsync_backend.sync_full(SyncRequest {
            dry_run: false,
            delete: config.sync.delete,
            project_root: local_root.clone(),
        })?;
        println!("{}", report.format_text());
    }
    let (sender, receiver) = mpsc::channel();
    install_stdin_shutdown(Arc::clone(&shutdown));
    let mut watcher = build_watcher(request.poll, sender)?;
    for watch_dir in &config.sync.watch_dirs {
        let watch_path = local_root.join(watch_dir);
        watcher
            .watch(&watch_path, RecursiveMode::Recursive)
            .map_err(|source| {
                err_with_source(error_info::WATCH_EVENT_FAILED, source)
                    .with_path(watch_path.display())
            })?;
        println!("[watch] {}", watch_path.display());
    }
    println!("[watch] press q then Enter to stop");

    let debounce = Duration::from_millis(config.sync.debounce_ms.max(1000));
    let mut pending = PendingChanges::default();
    let mut last_event_at = Instant::now();
    while !shutdown.load(Ordering::SeqCst) && !stop_requested(&request.project_root) {
        let timeout = if pending.has_changes() {
            debounce.saturating_sub(last_event_at.elapsed())
        } else {
            Duration::from_millis(500)
        };

        match receiver.recv_timeout(timeout) {
            Ok(event) => {
                if let Some(changes) =
                    collect_event_changes(&event, &local_root, &config.sync.exclude)
                {
                    pending.merge(changes);
                    last_event_at = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pending.has_changes() && last_event_at.elapsed() >= debounce {
                    let changes = pending.take();
                    let changes = reconcile_existing_paths(changes, &local_root);
                    let report = delta_backend.sync_delta(SyncDeltaRequest {
                        project_root: local_root.clone(),
                        uploads: changes.uploads.into_iter().collect(),
                        deletes: changes.deletes.into_iter().collect(),
                    })?;
                    println!("{}", report.format_text());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(err(error_info::WATCH_EVENT_FAILED).with_hint("file watcher stopped"));
            }
        }
    }
    println!("[watch] stopped");
    clear_pid_file(&request.project_root)?;
    Ok(())
}

pub fn request_stop(project_root: &Path) -> Result<()> {
    let dir = project_root.join(RDEV_DIR);
    fs::create_dir_all(&dir)
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    fs::write(dir.join(STOP_FILE), b"stop")
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if let Some(pid) = read_pid_file(project_root)? {
        terminate_pid(pid)?;
    }
    Ok(())
}

fn clear_stop_request(project_root: &Path) -> Result<()> {
    let path = stop_file(project_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(err_with_source(error_info::WATCH_EVENT_FAILED, source)),
    }
}

fn stop_requested(project_root: &Path) -> bool {
    stop_file(project_root).exists()
}

fn stop_file(project_root: &Path) -> PathBuf {
    project_root.join(RDEV_DIR).join(STOP_FILE)
}

fn write_pid_file(project_root: &Path) -> Result<()> {
    let dir = project_root.join(RDEV_DIR);
    fs::create_dir_all(&dir)
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    fs::write(pid_file(project_root), std::process::id().to_string())
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))
}

fn clear_pid_file(project_root: &Path) -> Result<()> {
    let path = pid_file(project_root);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(err_with_source(error_info::WATCH_EVENT_FAILED, source)),
    }
}

fn read_pid_file(project_root: &Path) -> Result<Option<u32>> {
    let path = pid_file(project_root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(err_with_source(error_info::WATCH_EVENT_FAILED, source)),
    };
    Ok(raw.trim().parse::<u32>().ok())
}

fn pid_file(project_root: &Path) -> PathBuf {
    project_root.join(RDEV_DIR).join(PID_FILE)
}

fn terminate_pid(pid: u32) -> Result<()> {
    if pid == std::process::id() {
        return Ok(());
    }
    terminate_other_pid(pid)
}

#[cfg(windows)]
fn terminate_other_pid(pid: u32) -> Result<()> {
    let output = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/T")
        .arg("/F")
        .output()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(err(error_info::WATCH_EVENT_FAILED)
            .with_hint(String::from_utf8_lossy(&output.stderr).trim()))
    }
}

#[cfg(not(windows))]
fn terminate_other_pid(pid: u32) -> Result<()> {
    let output = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output()
        .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(err(error_info::WATCH_EVENT_FAILED)
            .with_hint(String::from_utf8_lossy(&output.stderr).trim()))
    }
}

fn install_shutdown_handler() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
    })
    .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
    Ok(shutdown)
}

fn install_stdin_shutdown(shutdown: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) if line.trim().eq_ignore_ascii_case("q") => {
                    shutdown.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(_) => {}
            }
        }
    });
}

fn build_watcher(poll: bool, sender: mpsc::Sender<Event>) -> Result<RecommendedWatcher> {
    let callback = move |result: notify::Result<Event>| {
        if let Ok(event) = result {
            let _send_result = sender.send(event);
        }
    };

    if poll {
        RecommendedWatcher::new(
            callback,
            Config::default().with_poll_interval(Duration::from_secs(1)),
        )
    } else {
        RecommendedWatcher::new(callback, Config::default())
    }
    .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))
}

fn resolve_local_root(project_root: &Path, local_path: &Path) -> PathBuf {
    if local_path.is_absolute() {
        local_path.to_path_buf()
    } else {
        project_root.join(local_path)
    }
}

#[derive(Debug, Default)]
struct PendingChanges {
    uploads: BTreeSet<PathBuf>,
    deletes: BTreeSet<PathBuf>,
}

impl PendingChanges {
    fn has_changes(&self) -> bool {
        !self.uploads.is_empty() || !self.deletes.is_empty()
    }

    fn merge(&mut self, other: Self) {
        for upload in other.uploads {
            self.deletes.remove(&upload);
            self.uploads.insert(upload);
        }
        for delete in other.deletes {
            self.uploads.remove(&delete);
            self.deletes.insert(delete);
        }
    }

    fn take(&mut self) -> Self {
        Self {
            uploads: std::mem::take(&mut self.uploads),
            deletes: std::mem::take(&mut self.deletes),
        }
    }
}

fn collect_event_changes(
    event: &Event,
    local_root: &Path,
    excludes: &[String],
) -> Option<PendingChanges> {
    let mut changes = PendingChanges::default();
    for path in &event.paths {
        if is_excluded(path, local_root, excludes) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(local_root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        match event.kind {
            EventKind::Remove(_) => {
                changes.deletes.insert(relative.to_path_buf());
            }
            _ => {
                changes.uploads.insert(relative.to_path_buf());
            }
        }
    }
    if changes.has_changes() {
        Some(changes)
    } else {
        None
    }
}

fn reconcile_existing_paths(mut changes: PendingChanges, local_root: &Path) -> PendingChanges {
    let uploads = std::mem::take(&mut changes.uploads);
    for upload in uploads {
        if local_root.join(&upload).exists() {
            changes.uploads.insert(upload);
        } else {
            changes.deletes.insert(upload);
        }
    }
    changes
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use notify::{Event, EventKind};

    use super::{
        collect_event_changes, is_excluded, reconcile_existing_paths, request_stop,
        resolve_local_root, stop_requested,
    };

    #[test]
    fn resolves_relative_local_root() {
        let root = resolve_local_root(
            &PathBuf::from("J:\\RustWorkspace\\project"),
            &PathBuf::from("."),
        );

        assert_eq!(root, PathBuf::from("J:\\RustWorkspace\\project").join("."));
    }

    #[test]
    fn filters_excluded_event_paths() {
        let local_root = PathBuf::from("J:\\project");
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("J:\\project\\data\\db")],
            attrs: notify::event::EventAttributes::new(),
        };

        assert!(collect_event_changes(&event, &local_root, &["data".to_owned()]).is_none());
        assert!(is_excluded(
            &PathBuf::from("J:\\project\\data\\db"),
            &local_root,
            &["data".to_owned()]
        ));
    }

    #[test]
    fn collects_upload_and_delete_changes() {
        let local_root = PathBuf::from("J:\\project");
        let modify = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("J:\\project\\src\\main.rs")],
            attrs: notify::event::EventAttributes::new(),
        };
        let remove = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![PathBuf::from("J:\\project\\src\\old.rs")],
            attrs: notify::event::EventAttributes::new(),
        };

        let upload = collect_event_changes(&modify, &local_root, &[]);
        let delete = collect_event_changes(&remove, &local_root, &[]);

        assert!(upload.is_some());
        assert!(delete.is_some());
        let delete = match delete {
            Some(delete) => delete,
            None => panic!("delete should be collected"),
        };
        assert!(delete.deletes.contains(&PathBuf::from("src\\old.rs")));
    }

    #[test]
    fn missing_pending_upload_becomes_delete() {
        let local_root = PathBuf::from("J:\\project");
        let mut changes = super::PendingChanges::default();
        changes.uploads.insert(PathBuf::from("src\\gone.rs"));

        let changes = reconcile_existing_paths(changes, &local_root);

        assert!(changes.uploads.is_empty());
        assert!(changes.deletes.contains(&PathBuf::from("src\\gone.rs")));
    }

    #[test]
    fn stop_file_requests_shutdown() {
        let root = std::env::temp_dir().join(format!("rdev-stop-test-{}", std::process::id()));
        let _cleanup_before = std::fs::remove_dir_all(&root);
        let result = request_stop(&root);

        assert!(result.is_ok());
        assert!(stop_requested(&root));

        let _cleanup_after = std::fs::remove_dir_all(&root);
    }
}
