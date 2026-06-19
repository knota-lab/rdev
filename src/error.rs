use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::panic::Location;

pub type Result<T> = std::result::Result<T, RdevError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSeverity {
    User,
    Environment,
    Remote,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum RdevExitCode {
    Success = 0,
    Config = 10,
    MissingTool = 20,
    Remote = 30,
    Sync = 40,
    Watch = 50,
    Internal = 70,
}

impl RdevExitCode {
    pub const fn code(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorInfo {
    pub code: &'static str,
    pub message_zh: &'static str,
    pub message_en: &'static str,
    pub severity: ErrorSeverity,
    pub exit_code: RdevExitCode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorContext {
    pub command: Option<String>,
    pub path: Option<String>,
    pub remote: Option<String>,
    pub exit_code: Option<i32>,
}

impl ErrorContext {
    pub const fn empty() -> Self {
        Self {
            command: None,
            path: None,
            remote: None,
            exit_code: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorLocation {
    pub file: &'static str,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug)]
pub struct RdevError(Box<RdevErrorInner>);

#[derive(Debug)]
pub struct RdevErrorInner {
    pub info: ErrorInfo,
    pub message: String,
    pub hint: Option<String>,
    pub context: ErrorContext,
    pub location: ErrorLocation,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl RdevError {
    pub fn new(inner: RdevErrorInner) -> Self {
        Self(Box::new(inner))
    }

    pub fn exit_code(&self) -> i32 {
        self.context
            .exit_code
            .unwrap_or_else(|| self.info.exit_code.code())
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.0.hint = Some(hint.into());
        self
    }

    pub fn with_path(mut self, path: impl fmt::Display) -> Self {
        self.0.context.path = Some(path.to_string());
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.0.context.command = Some(command.into());
        self
    }

    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.0.context.remote = Some(remote.into());
        self
    }

    pub fn with_exit_code(mut self, exit_code: Option<i32>) -> Self {
        self.0.context.exit_code = exit_code;
        self
    }
}

impl Deref for RdevError {
    type Target = RdevErrorInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for RdevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.info.code, self.message)
    }
}

impl Error for RdevError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[track_caller]
pub fn err(info: ErrorInfo) -> RdevError {
    make_error(info, None)
}

#[track_caller]
pub fn err_with_source<E>(info: ErrorInfo, source: E) -> RdevError
where
    E: std::error::Error + Send + Sync + 'static,
{
    make_error(info, Some(Box::new(source)))
}

#[track_caller]
fn make_error(
    info: ErrorInfo,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
) -> RdevError {
    let caller = Location::caller();
    RdevError::new(RdevErrorInner {
        info,
        message: info.message_zh.to_owned(),
        hint: None,
        context: ErrorContext::empty(),
        location: ErrorLocation {
            file: caller.file(),
            line: caller.line(),
            column: caller.column(),
        },
        source,
    })
}

pub fn format_error(error: &RdevError) -> String {
    let mut output = format!("[error] {}: {}", error.info.code, error.message);
    if let Some(hint) = &error.hint {
        output.push_str(&format!("\n[hint] {hint}"));
    }
    if let Some(source) = error.source() {
        output.push_str(&format!("\n[source] {source}"));
    }
    append_context(&mut output, "command", error.context.command.as_deref());
    append_context(&mut output, "path", error.context.path.as_deref());
    append_context(&mut output, "remote", error.context.remote.as_deref());
    if let Some(exit_code) = error.context.exit_code {
        output.push_str(&format!("\n[context] exit_code={exit_code}"));
    }
    output
}

fn append_context(output: &mut String, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        output.push_str(&format!("\n[context] {key}={value}"));
    }
}

pub fn localized_message(info: ErrorInfo, language: Language) -> Cow<'static, str> {
    match language {
        Language::ZhCn => Cow::Borrowed(info.message_zh),
        Language::EnUs => Cow::Borrowed(info.message_en),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    ZhCn,
    EnUs,
}

#[cfg(test)]
mod tests {
    use super::{err, localized_message, Language};
    use crate::error_info;

    #[test]
    fn error_info_controls_exit_code() {
        let error = err(error_info::CONFIG_NOT_FOUND);

        assert_eq!(error.exit_code(), 10);
        assert_eq!(error.info.code, "config.not_found");
    }

    #[test]
    fn context_exit_code_overrides_error_info_exit_code() {
        let error = err(error_info::REMOTE_COMMAND_FAILED).with_exit_code(Some(130));

        assert_eq!(error.exit_code(), 130);
    }

    #[test]
    fn formats_hint_and_context() {
        let error = err(error_info::CONFIG_INVALID_REMOTE_PATH)
            .with_hint("use a project path")
            .with_path("/root");

        let output = super::format_error(&error);

        assert!(output.contains("config.invalid_remote_path"));
        assert!(output.contains("use a project path"));
        assert!(output.contains("path=/root"));
    }

    #[test]
    fn returns_localized_message() {
        let zh = localized_message(error_info::CONFIG_NOT_FOUND, Language::ZhCn);
        let en = localized_message(error_info::CONFIG_NOT_FOUND, Language::EnUs);

        assert_eq!(zh, "未找到 .rdev/config.toml 配置文件");
        assert_eq!(en, "Could not find .rdev/config.toml");
    }
}
