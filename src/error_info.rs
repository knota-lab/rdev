use crate::error::{ErrorInfo, ErrorSeverity, RdevExitCode};

pub const CONFIG_NOT_FOUND: ErrorInfo = ErrorInfo {
    code: "config.not_found",
    message_zh: "未找到 .rdev.toml 配置文件",
    message_en: "Could not find .rdev.toml",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Config,
};

pub const CONFIG_INVALID: ErrorInfo = ErrorInfo {
    code: "config.invalid",
    message_zh: "配置文件无效",
    message_en: "Configuration file is invalid",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Config,
};

pub const CONFIG_INVALID_REMOTE_PATH: ErrorInfo = ErrorInfo {
    code: "config.invalid_remote_path",
    message_zh: "远端目录不安全",
    message_en: "Remote path is unsafe",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Config,
};

pub const PATH_ESCAPE_DENIED: ErrorInfo = ErrorInfo {
    code: "path.escape_denied",
    message_zh: "路径不能逃逸项目根目录",
    message_en: "Path must stay inside the project root",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Config,
};

pub const TOOL_SSH_NOT_FOUND: ErrorInfo = ErrorInfo {
    code: "tool.ssh_not_found",
    message_zh: "未找到本地 ssh 工具",
    message_en: "Could not find local ssh tool",
    severity: ErrorSeverity::Environment,
    exit_code: RdevExitCode::MissingTool,
};

pub const TOOL_RSYNC_NOT_FOUND: ErrorInfo = ErrorInfo {
    code: "tool.rsync_not_found",
    message_zh: "未找到本地 rsync 工具",
    message_en: "Could not find local rsync tool",
    severity: ErrorSeverity::Environment,
    exit_code: RdevExitCode::MissingTool,
};

pub const REMOTE_SSH_CONNECT_FAILED: ErrorInfo = ErrorInfo {
    code: "remote.ssh_connect_failed",
    message_zh: "SSH 连接失败",
    message_en: "SSH connection failed",
    severity: ErrorSeverity::Remote,
    exit_code: RdevExitCode::Remote,
};

pub const REMOTE_COMMAND_FAILED: ErrorInfo = ErrorInfo {
    code: "remote.command_failed",
    message_zh: "远程命令执行失败",
    message_en: "Remote command failed",
    severity: ErrorSeverity::Remote,
    exit_code: RdevExitCode::Remote,
};

pub const REMOTE_PATH_NOT_WRITABLE: ErrorInfo = ErrorInfo {
    code: "remote.path_not_writable",
    message_zh: "远端目录不可写",
    message_en: "Remote path is not writable",
    severity: ErrorSeverity::Remote,
    exit_code: RdevExitCode::Remote,
};

pub const SYNC_RSYNC_FAILED: ErrorInfo = ErrorInfo {
    code: "sync.rsync_failed",
    message_zh: "rsync 同步失败",
    message_en: "rsync sync failed",
    severity: ErrorSeverity::Remote,
    exit_code: RdevExitCode::Sync,
};

pub const SYNC_DELETE_REJECTED: ErrorInfo = ErrorInfo {
    code: "sync.delete_rejected",
    message_zh: "删除动作被路径保护拒绝",
    message_en: "Delete action was rejected by path protection",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Sync,
};

pub const WATCH_EVENT_FAILED: ErrorInfo = ErrorInfo {
    code: "watch.event_failed",
    message_zh: "文件监听事件处理失败",
    message_en: "Failed to process file watch event",
    severity: ErrorSeverity::Environment,
    exit_code: RdevExitCode::Watch,
};

pub const INTERNAL_UNEXPECTED: ErrorInfo = ErrorInfo {
    code: "internal.unexpected",
    message_zh: "发生未预期的内部错误",
    message_en: "Unexpected internal error",
    severity: ErrorSeverity::Internal,
    exit_code: RdevExitCode::Internal,
};
