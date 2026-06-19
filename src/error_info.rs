use crate::error::{ErrorInfo, ErrorSeverity, RdevExitCode};

pub const CONFIG_NOT_FOUND: ErrorInfo = ErrorInfo {
    code: "config.not_found",
    message_zh: "未找到 .rdev/config.toml 配置文件",
    message_en: "Could not find .rdev/config.toml",
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

pub const REMOTE_TAR_NOT_FOUND: ErrorInfo = ErrorInfo {
    code: "remote.tar_not_found",
    message_zh: "远端未找到 tar 工具",
    message_en: "Remote tar tool not found",
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

pub const SYNC_SFTP_FAILED: ErrorInfo = ErrorInfo {
    code: "sync.sftp_failed",
    message_zh: "SFTP 同步失败",
    message_en: "SFTP sync failed",
    severity: ErrorSeverity::Remote,
    exit_code: RdevExitCode::Sync,
};

pub const SYNC_SSH_TAR_FAILED: ErrorInfo = ErrorInfo {
    code: "sync.ssh_tar_failed",
    message_zh: "SSH Tar 同步失败",
    message_en: "SSH tar sync failed",
    severity: ErrorSeverity::Remote,
    exit_code: RdevExitCode::Sync,
};

pub const SYNC_CANCELLED: ErrorInfo = ErrorInfo {
    code: "sync.cancelled",
    message_zh: "同步已取消",
    message_en: "Sync was cancelled",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Sync,
};

pub const SYNC_DELETE_REJECTED: ErrorInfo = ErrorInfo {
    code: "sync.delete_rejected",
    message_zh: "删除动作被路径保护拒绝",
    message_en: "Delete action was rejected by path protection",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Sync,
};

pub const SESSION_FAILED: ErrorInfo = ErrorInfo {
    code: "session.failed",
    message_zh: "会话操作失败",
    message_en: "Session operation failed",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Watch,
};

pub const DAEMON_FAILED: ErrorInfo = ErrorInfo {
    code: "daemon.failed",
    message_zh: "daemon 操作失败",
    message_en: "Daemon operation failed",
    severity: ErrorSeverity::Environment,
    exit_code: RdevExitCode::Watch,
};

pub const DAEMON_NOT_RUNNING: ErrorInfo = ErrorInfo {
    code: "daemon.not_running",
    message_zh: "daemon 未运行",
    message_en: "Daemon is not running",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Watch,
};

pub const DAEMON_BUSY: ErrorInfo = ErrorInfo {
    code: "daemon.busy",
    message_zh: "daemon 当前有任务正在运行",
    message_en: "Daemon is busy",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Watch,
};

pub const DAEMON_PROTOCOL_ERROR: ErrorInfo = ErrorInfo {
    code: "daemon.protocol_error",
    message_zh: "daemon 协议错误",
    message_en: "Daemon protocol error",
    severity: ErrorSeverity::Environment,
    exit_code: RdevExitCode::Watch,
};

pub const DAEMON_EXEC_CANCELLED: ErrorInfo = ErrorInfo {
    code: "daemon.exec_cancelled",
    message_zh: "远程命令已取消",
    message_en: "Remote command was cancelled",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Watch,
};

pub const WATCH_EVENT_FAILED: ErrorInfo = ErrorInfo {
    code: "watch.event_failed",
    message_zh: "文件监听事件处理失败",
    message_en: "Failed to process file watch event",
    severity: ErrorSeverity::Environment,
    exit_code: RdevExitCode::Watch,
};

pub const WATCH_ALREADY_RUNNING: ErrorInfo = ErrorInfo {
    code: "watch.already_running",
    message_zh: "当前项目已有 up 进程在运行",
    message_en: "An up process is already running for this project",
    severity: ErrorSeverity::User,
    exit_code: RdevExitCode::Watch,
};

pub const INTERNAL_UNEXPECTED: ErrorInfo = ErrorInfo {
    code: "internal.unexpected",
    message_zh: "发生未预期的内部错误",
    message_en: "Unexpected internal error",
    severity: ErrorSeverity::Internal,
    exit_code: RdevExitCode::Internal,
};
