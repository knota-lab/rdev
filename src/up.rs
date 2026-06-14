use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::config::AppConfig;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::process::ProcessRunner;
use crate::sync::{RsyncSyncBackend, SyncRequest};

#[derive(Debug, Clone)]
pub struct UpRequest {
    pub project_root: PathBuf,
    pub initial_sync: bool,
    pub poll: bool,
}

pub fn run_up(config: &AppConfig, runner: &impl ProcessRunner, request: UpRequest) -> Result<()> {
    let local_root = resolve_local_root(&request.project_root, &config.sync.local_path);
    let backend = RsyncSyncBackend::new(config, runner);
    if request.initial_sync {
        let report = backend.sync_full(SyncRequest {
            dry_run: false,
            delete: config.sync.delete,
            project_root: local_root.clone(),
        })?;
        println!("{}", report.format_text());
    }

    let (sender, receiver) = mpsc::channel();
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

    let debounce = Duration::from_millis(config.sync.debounce_ms);
    let mut pending = false;
    let mut last_event_at = Instant::now();
    loop {
        let timeout = if pending {
            debounce.saturating_sub(last_event_at.elapsed())
        } else {
            Duration::from_millis(500)
        };

        match receiver.recv_timeout(timeout) {
            Ok(event) => {
                if should_process_event(&event, &local_root, &config.sync.exclude) {
                    pending = true;
                    last_event_at = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pending && last_event_at.elapsed() >= debounce {
                    let report = backend.sync_full(SyncRequest {
                        dry_run: false,
                        delete: config.sync.delete,
                        project_root: local_root.clone(),
                    })?;
                    println!("{}", report.format_text());
                    pending = false;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(err(error_info::WATCH_EVENT_FAILED).with_hint("file watcher stopped"));
            }
        }
    }
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

fn should_process_event(event: &Event, local_root: &Path, excludes: &[String]) -> bool {
    event
        .paths
        .iter()
        .any(|path| !is_excluded(path, local_root, excludes))
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

    use super::{is_excluded, resolve_local_root, should_process_event};

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

        assert!(!should_process_event(
            &event,
            &local_root,
            &["data".to_owned()]
        ));
        assert!(is_excluded(
            &PathBuf::from("J:\\project\\data\\db"),
            &local_root,
            &["data".to_owned()]
        ));
    }
}
