use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::error::{err, Result};
use crate::error_info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_remote_path(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn join_relative(&self, relative: &RelativePath) -> Self {
        let mut base = self.0.trim_end_matches('/').to_owned();
        if !relative.as_str().is_empty() {
            base.push('/');
            base.push_str(relative.as_str());
        }
        Self(base)
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativePath(String);

impl RelativePath {
    pub fn parse(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        validate_relative_path(path)?;
        let normalized = normalize_relative_path(path);
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct PathMapper {
    local_root: PathBuf,
    rsync_local_root: Option<String>,
    remote_root: RemotePath,
}

pub fn is_sync_excluded(path: &Path, local_root: &Path, excludes: &[String]) -> bool {
    explain_sync_exclusion(path, local_root, excludes).excluded
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncExclusionExplanation {
    pub relative_path: Option<String>,
    pub excluded: bool,
    pub reason: SyncExclusionReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncExclusionReason {
    OutsideLocalRoot,
    ProjectRoot,
    NoMatchingRule,
    MatchedRule {
        rule: String,
        include: bool,
        pattern: String,
    },
}

pub fn explain_sync_exclusion(
    path: &Path,
    local_root: &Path,
    excludes: &[String],
) -> SyncExclusionExplanation {
    let Ok(relative) = path.strip_prefix(local_root) else {
        return SyncExclusionExplanation {
            relative_path: None,
            excluded: true,
            reason: SyncExclusionReason::OutsideLocalRoot,
        };
    };
    let relative_path = Some(normalize_relative_path(relative));
    let components = relative_normal_components(relative);
    if components.is_empty() {
        return SyncExclusionExplanation {
            relative_path,
            excluded: false,
            reason: SyncExclusionReason::ProjectRoot,
        };
    }
    let mut excluded = false;
    let mut reason = SyncExclusionReason::NoMatchingRule;
    for raw_rule in excludes {
        let rule = parse_exclude_rule(raw_rule);
        if exclude_matches(&components, rule.pattern) {
            excluded = !rule.include;
            reason = SyncExclusionReason::MatchedRule {
                rule: raw_rule.clone(),
                include: rule.include,
                pattern: rule.pattern.to_owned(),
            };
        }
    }
    SyncExclusionExplanation {
        relative_path,
        excluded,
        reason,
    }
}

struct ExcludeRule<'a> {
    include: bool,
    pattern: &'a str,
}

fn parse_exclude_rule(rule: &str) -> ExcludeRule<'_> {
    let trimmed = rule.trim();
    let Some(pattern) = trimmed.strip_prefix('!') else {
        return ExcludeRule {
            include: false,
            pattern: trimmed,
        };
    };
    ExcludeRule {
        include: true,
        pattern: pattern.trim(),
    }
}

fn exclude_matches(components: &[String], pattern: &str) -> bool {
    let exclude_components = exclude_components(pattern);
    match exclude_components.as_slice() {
        [] => false,
        [name] => components.iter().any(|component| component == name),
        parts => components
            .windows(parts.len())
            .any(|window| window == parts),
    }
}

fn exclude_components(exclude: &str) -> Vec<String> {
    exclude
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != ".")
        .map(ToOwned::to_owned)
        .collect()
}

fn relative_normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

impl PathMapper {
    pub fn new(
        local_root: PathBuf,
        rsync_local_root: Option<String>,
        remote_root: RemotePath,
    ) -> Self {
        Self {
            local_root,
            rsync_local_root,
            remote_root,
        }
    }

    pub fn relative_to_local_abs(&self, path: &RelativePath) -> PathBuf {
        self.local_root.join(path.as_str())
    }

    pub fn relative_to_remote_abs(&self, path: &RelativePath) -> RemotePath {
        self.remote_root.join_relative(path)
    }

    pub fn relative_to_rsync_local(&self, path: &RelativePath) -> String {
        if let Some(root) = &self.rsync_local_root {
            let root = root.trim_end_matches('/');
            if path.as_str().is_empty() {
                root.to_owned()
            } else {
                format!("{root}/{}", path.as_str())
            }
        } else {
            self.relative_to_local_abs(path).display().to_string()
        }
    }
}

fn validate_remote_path(value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || !trimmed.starts_with('/')
        || trimmed.contains("..")
        || trimmed.contains('~')
        || trimmed.contains('$')
        || trimmed.contains('*')
        || is_broad_remote_path(trimmed)
    {
        return Err(err(error_info::CONFIG_INVALID_REMOTE_PATH)
            .with_path(value)
            .with_hint("remote.path 必须是明确的项目绝对路径，不能是 /、/root、/home 或 /tmp"));
    }
    Ok(())
}

fn is_broad_remote_path(value: &str) -> bool {
    matches!(value, "/" | "/root" | "/home" | "/tmp")
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(err(error_info::PATH_ESCAPE_DENIED).with_path(path.display()));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(err(error_info::PATH_ESCAPE_DENIED).with_path(path.display()));
            }
        }
    }
    Ok(())
}

fn normalize_relative_path(path: &Path) -> String {
    let mut parts = Vec::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            parts.push(part.to_string_lossy().to_string());
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        explain_sync_exclusion, is_sync_excluded, PathMapper, RelativePath, RemotePath,
        SyncExclusionReason,
    };

    #[test]
    fn rejects_broad_remote_paths() {
        for path in ["/", "/root", "/home", "/tmp", "relative", "/root/../tmp"] {
            assert!(
                RemotePath::parse(path).is_err(),
                "{path} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_project_remote_path() {
        let path = RemotePath::parse("/root/project");

        assert!(path.is_ok());
        let path = match path {
            Ok(path) => path,
            Err(error) => panic!("remote path should parse: {error}"),
        };
        assert_eq!(path.as_str(), "/root/project");
    }

    #[test]
    fn rejects_relative_path_escape() {
        for path in ["..", "../src", "/tmp", "C:\\tmp"] {
            assert!(
                RelativePath::parse(path).is_err(),
                "{path} should be rejected"
            );
        }
    }

    #[test]
    fn normalizes_relative_path_to_forward_slashes() {
        let path = RelativePath::parse(PathBuf::from("src").join("main.rs"));

        assert!(path.is_ok());
        let path = match path {
            Ok(path) => path,
            Err(error) => panic!("relative path should parse: {error}"),
        };
        assert_eq!(path.as_str(), "src/main.rs");
    }

    #[test]
    fn maps_relative_to_remote_path() {
        let remote_root = match RemotePath::parse("/root/project") {
            Ok(path) => path,
            Err(error) => panic!("remote path should parse: {error}"),
        };
        let mapper = PathMapper::new(PathBuf::from("J:\\project"), None, remote_root);
        let relative = match RelativePath::parse("src/main.rs") {
            Ok(path) => path,
            Err(error) => panic!("relative path should parse: {error}"),
        };

        let remote = mapper.relative_to_remote_abs(&relative);

        assert_eq!(remote.as_str(), "/root/project/src/main.rs");
    }

    #[test]
    fn excludes_matching_path_component() {
        let root = PathBuf::from("J:\\project");

        assert!(is_sync_excluded(
            &PathBuf::from("J:\\project\\data\\db"),
            &root,
            &["data".to_owned()]
        ));
        assert!(is_sync_excluded(
            &PathBuf::from("J:\\project\\src\\data\\db"),
            &root,
            &["data".to_owned()]
        ));
    }

    #[test]
    fn include_rule_overrides_previous_exclude() {
        let root = PathBuf::from("J:\\workspace");
        let excludes = ["data".to_owned(), "!src/data".to_owned()];

        assert!(is_sync_excluded(
            &PathBuf::from("J:\\workspace\\knota-fold\\data\\db"),
            &root,
            &excludes
        ));
        assert!(!is_sync_excluded(
            &PathBuf::from("J:\\workspace\\knota-fold\\src\\data\\mod.rs"),
            &root,
            &excludes
        ));
        let explanation = explain_sync_exclusion(
            &PathBuf::from("J:\\workspace\\knota-fold\\src\\data\\mod.rs"),
            &root,
            &excludes,
        );
        assert!(!explanation.excluded);
        assert_eq!(
            explanation.reason,
            SyncExclusionReason::MatchedRule {
                rule: "!src/data".to_owned(),
                include: true,
                pattern: "src/data".to_owned(),
            }
        );
    }

    #[test]
    fn supports_path_excludes() {
        let root = PathBuf::from("J:\\workspace");

        assert!(is_sync_excluded(
            &PathBuf::from("J:\\workspace\\knota-fold\\data\\db"),
            &root,
            &["knota-fold/data".to_owned()]
        ));
        assert!(!is_sync_excluded(
            &PathBuf::from("J:\\workspace\\other\\data\\db"),
            &root,
            &["knota-fold/data".to_owned()]
        ));
        assert!(is_sync_excluded(
            &PathBuf::from("J:\\workspace\\other\\knota-fold\\data\\db"),
            &root,
            &["knota-fold/data".to_owned()]
        ));
    }
}
