use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{err_with_source, Result};
use crate::error_info;

pub const CONFIG_FILE_NAME: &str = ".rdev.toml";
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub version: u32,
    pub remote: RemoteConfig,
    pub sync: SyncConfig,
    pub command: CommandConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    #[serde(default)]
    pub ssh_config: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncConfig {
    pub local_path: PathBuf,
    pub watch_dirs: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub use_gitignore: bool,
    pub debounce_ms: u64,
    pub direction: SyncDirection,
    pub delete: bool,
    pub delete_policy: DeletePolicy,
    pub full_sync_threshold: usize,
    #[serde(default)]
    pub rsync_mode: RsyncMode,
    #[serde(default)]
    pub rsync_local_path: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncDirection {
    Push,
    Pull,
    Bidirectional,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeletePolicy {
    Propagate,
    KeepTarget,
    TrashTarget,
    Confirm,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RsyncMode {
    #[default]
    Auto,
    Native,
    Wsl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandConfig {
    pub default_shell: String,
    pub remote_env: BTreeMap<String, String>,
}

impl AppConfig {
    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join(CONFIG_FILE_NAME);
        let raw = fs::read_to_string(&path).map_err(|source| {
            err_with_source(error_info::CONFIG_NOT_FOUND, source).with_path(path.display())
        })?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        toml::from_str(raw).map_err(|source| err_with_source(error_info::CONFIG_INVALID, source))
    }

    pub fn template(host: &str, port: u16, remote_path: &str) -> Self {
        Self {
            version: CONFIG_VERSION,
            remote: RemoteConfig {
                host: host.to_owned(),
                port,
                path: remote_path.to_owned(),
                ssh_config: String::new(),
            },
            sync: SyncConfig::default(),
            command: CommandConfig::default(),
        }
    }
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            local_path: PathBuf::from("."),
            watch_dirs: vec![PathBuf::from(".")],
            exclude: vec![
                ".git".to_owned(),
                "target".to_owned(),
                "node_modules".to_owned(),
                ".rdev".to_owned(),
                ".idea".to_owned(),
                ".vscode".to_owned(),
                "dist".to_owned(),
                "build".to_owned(),
            ],
            use_gitignore: true,
            debounce_ms: 300,
            direction: SyncDirection::Push,
            delete: true,
            delete_policy: DeletePolicy::Propagate,
            full_sync_threshold: 32,
            rsync_mode: RsyncMode::Auto,
            rsync_local_path: String::new(),
        }
    }
}

impl Default for CommandConfig {
    fn default() -> Self {
        Self {
            default_shell: "bash".to_owned(),
            remote_env: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, DeletePolicy, SyncDirection, CONFIG_VERSION};

    #[test]
    fn parses_valid_config() {
        let raw = r#"
version = 1

[remote]
host = "root@example.com"
port = 22
path = "/root/project"

[sync]
local_path = "."
watch_dirs = ["."]
exclude = [".git", "target", ".rdev"]
use_gitignore = true
debounce_ms = 300
direction = "push"
delete = true
delete_policy = "propagate"
full_sync_threshold = 32
rsync_mode = "auto"

[command]
default_shell = "bash"
remote_env = {}
"#;

        let config = AppConfig::parse(raw);

        assert!(config.is_ok());
        let config = match config {
            Ok(config) => config,
            Err(error) => panic!("config should parse: {error}"),
        };
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.remote.host, "root@example.com");
        assert_eq!(config.sync.direction, SyncDirection::Push);
        assert_eq!(config.sync.delete_policy, DeletePolicy::Propagate);
        assert_eq!(config.sync.rsync_mode, super::RsyncMode::Auto);
    }

    #[test]
    fn template_uses_safe_defaults() {
        let config = AppConfig::template("root@example.com", 2222, "/root/project");

        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.remote.port, 2222);
        assert!(config.sync.exclude.iter().any(|item| item == ".rdev"));
        assert_eq!(config.sync.full_sync_threshold, 32);
        assert_eq!(config.sync.rsync_mode, super::RsyncMode::Auto);
    }
}
