use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::CONFIG_DIR_NAME;
use crate::error::{err, err_with_source, Result};
use crate::error_info;

const TUI_STATE_FILE_NAME: &str = "tui-state.toml";

#[derive(Debug, Clone)]
pub(super) struct TuiStateStore {
    path: PathBuf,
    state: TuiState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct TuiState {
    #[serde(default)]
    command_history: Vec<String>,
    #[serde(default)]
    sessions: Vec<SavedSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct SavedSession {
    pub(super) name: String,
    pub(super) kind: SavedSessionKind,
    pub(super) command: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum SavedSessionKind {
    Local,
    Remote,
}

impl TuiStateStore {
    pub(super) fn load(project_root: &Path) -> (Self, Option<String>) {
        let path = project_root.join(CONFIG_DIR_NAME).join(TUI_STATE_FILE_NAME);
        let loaded = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| toml::from_str::<TuiState>(&raw).ok());
        let event = if path.exists() && loaded.is_none() {
            Some(format!(
                "ignored invalid TUI state file: {}",
                path.display()
            ))
        } else {
            None
        };
        (
            Self {
                path,
                state: loaded.unwrap_or_default(),
            },
            event,
        )
    }

    pub(super) fn command_history(&self) -> &[String] {
        &self.state.command_history
    }

    pub(super) fn push_command_history(&mut self, command: &str, limit: usize) -> Result<()> {
        if command.is_empty() {
            return Ok(());
        }
        if self
            .state
            .command_history
            .last()
            .is_some_and(|previous| previous == command)
        {
            return Ok(());
        }
        self.state.command_history.push(command.to_owned());
        if self.state.command_history.len() > limit {
            let overflow = self.state.command_history.len() - limit;
            self.state.command_history.drain(0..overflow);
        }
        self.save()
    }

    pub(super) fn remember_session(&mut self, session: SavedSession) -> Result<()> {
        if let Some(existing) = self
            .state
            .sessions
            .iter_mut()
            .find(|existing| existing.name == session.name)
        {
            *existing = session;
        } else {
            self.state.sessions.push(session);
        }
        self.save()
    }

    pub(super) fn delete_session(&mut self, selector: &str) -> Result<String> {
        let Some(index) = self.session_index(selector) else {
            return Err(err(error_info::SESSION_FAILED)
                .with_hint(format!("saved session not found: {selector}")));
        };
        let removed = self.state.sessions.remove(index);
        self.save()?;
        Ok(format!("deleted saved session {}", removed.name))
    }

    pub(super) fn saved_sessions_text(&self) -> String {
        if self.state.sessions.is_empty() {
            return "saved sessions: <empty>".to_owned();
        }
        self.state
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                format!(
                    "{}. {} {} -- {}",
                    index + 1,
                    session.kind.label(),
                    session.name,
                    session.command
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn find_session(&self, selector: &str) -> Option<SavedSession> {
        self.session_index(selector)
            .and_then(|index| self.state.sessions.get(index).cloned())
    }

    fn session_index(&self, selector: &str) -> Option<usize> {
        let selector = selector.trim();
        if selector.is_empty() {
            return None;
        }
        if let Ok(number) = selector.parse::<usize>() {
            return number
                .checked_sub(1)
                .filter(|index| *index < self.state.sessions.len());
        }
        self.state
            .sessions
            .iter()
            .position(|session| session.name == selector)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
        }
        let raw = toml::to_string_pretty(&self.state)
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
        fs::write(&self.path, raw)
            .map_err(|source| err_with_source(error_info::WATCH_EVENT_FAILED, source))?;
        Ok(())
    }
}

impl SavedSessionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "session",
            Self::Remote => "remote-session",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SavedSessionKind, TuiStateStore};

    #[test]
    fn saved_session_can_be_found_by_name_or_index() {
        let (mut store, _) = TuiStateStore::load(&std::env::temp_dir().join("rdev-state-test"));
        store.state.sessions.clear();
        store
            .remember_session(super::SavedSession {
                name: "web".to_owned(),
                kind: SavedSessionKind::Remote,
                command: "pnpm dev".to_owned(),
            })
            .unwrap_or_else(|error| panic!("{error}"));

        assert_eq!(
            store.find_session("web").map(|session| session.command),
            Some("pnpm dev".to_owned())
        );
        assert_eq!(
            store.find_session("1").map(|session| session.name),
            Some("web".to_owned())
        );
    }
}
