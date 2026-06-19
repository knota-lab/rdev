use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::CONFIG_DIR_NAME;
use crate::error::{err_with_source, Result};
use crate::error_info;

const LOG_DIR_NAME: &str = "logs";
const TAIL_LIMIT: usize = 40;

pub(crate) struct ExecSummaryRecorder {
    command: String,
    dir: Option<String>,
    path: PathBuf,
    file: File,
    tail: VecDeque<String>,
    line_buffer: String,
    first_issue: Option<String>,
    line_count: usize,
    byte_count: usize,
}

impl ExecSummaryRecorder {
    pub(crate) fn new(project_root: &Path, command: String, dir: Option<String>) -> Result<Self> {
        let log_dir = project_root.join(CONFIG_DIR_NAME).join(LOG_DIR_NAME);
        fs::create_dir_all(&log_dir)
            .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
        let path = log_dir.join(format!("exec-{}.log", now_ms()));
        let file = File::create(&path)
            .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
        Ok(Self {
            command,
            dir,
            path,
            file,
            tail: VecDeque::new(),
            line_buffer: String::new(),
            first_issue: None,
            line_count: 0,
            byte_count: 0,
        })
    }

    pub(crate) fn record(&mut self, data: &str) -> Result<()> {
        self.file
            .write_all(data.as_bytes())
            .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
        self.byte_count += data.len();
        self.feed_lines(data);
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn byte_count(&self) -> usize {
        self.byte_count
    }

    pub(crate) fn finish(mut self, exit_code: i32) -> Result<String> {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.push_line(line);
        }
        self.file
            .flush()
            .map_err(|source| err_with_source(error_info::DAEMON_FAILED, source))?;
        Ok(self.format(exit_code))
    }

    fn feed_lines(&mut self, data: &str) {
        for chunk in data.split_inclusive('\n') {
            self.line_buffer.push_str(chunk);
            if chunk.ends_with('\n') {
                let line = std::mem::take(&mut self.line_buffer);
                self.push_line(line);
            }
        }
    }

    fn push_line(&mut self, line: String) {
        let line = line.trim_end_matches(['\r', '\n']).to_owned();
        self.line_count += 1;
        if self.first_issue.is_none() && is_issue_line(&line) {
            self.first_issue = Some(line.clone());
        }
        if self.tail.len() == TAIL_LIMIT {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    fn format(&self, exit_code: i32) -> String {
        let dir = self.dir.as_deref().unwrap_or(".");
        let first_issue = self.first_issue.as_deref().unwrap_or("<none>");
        let mut lines = vec![
            "[summary] remote exec completed".to_owned(),
            format!("[summary] exit_code={exit_code}"),
            format!("[summary] dir={dir}"),
            format!("[summary] command={}", self.command),
            format!("[summary] log={}", self.path.display()),
            format!(
                "[summary] captured lines={} bytes={}",
                self.line_count, self.byte_count
            ),
            format!("[summary] first_issue={first_issue}"),
        ];
        if !self.tail.is_empty() {
            lines.push(format!("[summary] last {} line(s):", self.tail.len()));
            for line in &self.tail {
                lines.push(format!("  {line}"));
            }
        }
        lines.join("\n")
    }
}

fn is_issue_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error:")
        || lower.contains("error[")
        || lower.contains("warning:")
        || lower.contains("warn:")
        || lower.contains("failed")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::{is_issue_line, ExecSummaryRecorder};

    #[test]
    fn detects_common_issue_lines() {
        assert!(is_issue_line("error[E0425]: cannot find value"));
        assert!(is_issue_line("warning: unused import"));
        assert!(is_issue_line("task failed"));
        assert!(!is_issue_line("finished dev profile"));
    }

    #[test]
    fn recorder_writes_log_and_formats_summary() {
        let root =
            std::env::temp_dir().join(format!("rdev-exec-summary-test-{}", std::process::id()));
        let _cleanup_before = std::fs::remove_dir_all(&root);
        if let Err(error) = std::fs::create_dir_all(&root) {
            panic!("create dir: {error}");
        }
        let mut recorder = match ExecSummaryRecorder::new(
            &root,
            "cargo test".to_owned(),
            Some("backend".to_owned()),
        ) {
            Ok(recorder) => recorder,
            Err(error) => panic!("create recorder: {error}"),
        };

        if let Err(error) = recorder.record("running\nerror[E0001]: failed\n") {
            panic!("record output: {error}");
        }
        let summary = match recorder.finish(101) {
            Ok(summary) => summary,
            Err(error) => panic!("finish summary: {error}"),
        };

        assert!(summary.contains("exit_code=101"));
        assert!(summary.contains("dir=backend"));
        assert!(summary.contains("command=cargo test"));
        assert!(summary.contains("first_issue=error[E0001]: failed"));
        assert!(summary.contains(".rdev"));
        let log_dir = root.join(".rdev").join("logs");
        let entries = match std::fs::read_dir(&log_dir) {
            Ok(entries) => entries.count(),
            Err(error) => panic!("read log dir: {error}"),
        };
        assert_eq!(entries, 1);
        let _cleanup_after = std::fs::remove_dir_all(&root);
    }
}
