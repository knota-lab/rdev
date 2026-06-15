use crate::config::{AppConfig, RsyncMode, SyncDirection};
use crate::error::{err, err_with_source, ErrorInfo, Result};
use crate::error_info;
use crate::path::RemotePath;
use crate::process::{ProcessCommand, ProcessRunner};
use crate::sftp::SftpDeltaBackend;

const REMOTE_BASE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn new(checks: Vec<DoctorCheck>) -> Self {
        Self { checks }
    }

    pub fn format_text(&self) -> String {
        self.checks
            .iter()
            .map(DoctorCheck::format_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    name: &'static str,
    detail: String,
}

impl DoctorCheck {
    pub fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            detail: detail.into(),
        }
    }

    fn format_text(&self) -> String {
        format!("[doctor] {} ok: {}", self.name, self.detail)
    }
}

pub fn run_doctor(config: &AppConfig, runner: &impl ProcessRunner) -> Result<DoctorReport> {
    let ssh_backend = SftpDeltaBackend::new(config);
    run_doctor_with_exec(config, runner, &ssh_backend)
}

pub trait RemoteExecChecker {
    fn check_exec(&self) -> Result<()>;
}

fn run_doctor_with_exec(
    config: &AppConfig,
    runner: &impl ProcessRunner,
    exec_checker: &impl RemoteExecChecker,
) -> Result<DoctorReport> {
    let mut checks = Vec::new();
    validate_config(config)?;
    checks.push(DoctorCheck::ok("config", "loaded"));

    let remote_path = RemotePath::parse(config.remote.path.as_str())?;
    checks.push(DoctorCheck::ok("remote.path", remote_path.as_str()));

    check_tool(
        runner,
        ToolCheck::new("ssh", error_info::TOOL_SSH_NOT_FOUND),
    )?;
    checks.push(DoctorCheck::ok("ssh", "available"));

    let rsync = check_rsync(runner, config.sync.rsync_mode)?;
    checks.push(DoctorCheck::ok("rsync", rsync.detail()));

    let remote = RemoteTarget::from_config(config);
    check_remote_sh(runner, &remote)?;
    checks.push(DoctorCheck::ok("remote.sh", "available"));

    check_remote_tar(runner, &remote)?;
    checks.push(DoctorCheck::ok("remote.tar", "available"));

    check_remote_rsync(runner, &remote)?;
    checks.push(DoctorCheck::ok("remote.rsync", "available"));

    exec_checker.check_exec()?;
    checks.push(DoctorCheck::ok("remote.ssh_exec", "available"));

    check_remote_path_writable(runner, &remote, &remote_path)?;
    checks.push(DoctorCheck::ok(
        "remote.path.writable",
        remote_path.as_str(),
    ));

    Ok(DoctorReport::new(checks))
}

impl RemoteExecChecker for SftpDeltaBackend<'_> {
    fn check_exec(&self) -> Result<()> {
        SftpDeltaBackend::check_exec(self)
    }
}

fn validate_config(config: &AppConfig) -> Result<()> {
    if config.sync.direction != SyncDirection::Push {
        return Err(err(error_info::CONFIG_INVALID)
            .with_hint("sync.direction 目前只支持 push，pull/bidirectional 为预留值"));
    }
    Ok(())
}

struct ToolCheck {
    name: &'static str,
    missing: ErrorInfo,
}

impl ToolCheck {
    fn new(name: &'static str, missing: ErrorInfo) -> Self {
        Self { name, missing }
    }

    fn version_command(&self) -> ProcessCommand {
        match self.name {
            "ssh" => ProcessCommand::new(self.name).arg("-V"),
            _ => ProcessCommand::new(self.name).arg("--version"),
        }
    }

    fn install_hint(&self) -> &'static str {
        match self.name {
            "ssh" => "请安装或启用 OpenSSH，并确认 ssh 在 PATH 中",
            "rsync" => "请通过 WSL、MSYS2、Git Bash 或 Cygwin 提供 rsync，并确认 rsync 在 PATH 中",
            _ => "请安装缺失工具，并确认它在 PATH 中",
        }
    }
}

fn check_tool(runner: &impl ProcessRunner, tool: ToolCheck) -> Result<()> {
    let command = tool.version_command();
    let display = command.display();
    match runner.output(command) {
        Ok(output) if output.code == Some(0) => Ok(()),
        Ok(output) => {
            Err(err(tool.missing)
                .with_command(display)
                .with_hint(if output.stderr.is_empty() {
                    tool.install_hint().to_owned()
                } else {
                    output.stderr
                }))
        }
        Err(source) => Err(err_with_source(tool.missing, source)
            .with_command(display)
            .with_hint(tool.install_hint())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectedRsync {
    Native,
    Wsl,
}

impl DetectedRsync {
    fn detail(self) -> &'static str {
        match self {
            Self::Native => "available via native PATH",
            Self::Wsl => "available via WSL",
        }
    }
}

fn check_rsync(runner: &impl ProcessRunner, mode: RsyncMode) -> Result<DetectedRsync> {
    match mode {
        RsyncMode::Native => {
            check_tool(
                runner,
                ToolCheck::new("rsync", error_info::TOOL_RSYNC_NOT_FOUND),
            )?;
            Ok(DetectedRsync::Native)
        }
        RsyncMode::Wsl => {
            check_wsl_rsync(runner)?;
            Ok(DetectedRsync::Wsl)
        }
        RsyncMode::Auto => match check_tool(
            runner,
            ToolCheck::new("rsync", error_info::TOOL_RSYNC_NOT_FOUND),
        ) {
            Ok(()) => Ok(DetectedRsync::Native),
            Err(_) => {
                check_wsl_rsync(runner)?;
                Ok(DetectedRsync::Wsl)
            }
        },
    }
}

fn check_wsl_rsync(runner: &impl ProcessRunner) -> Result<()> {
    let command = ProcessCommand::new("wsl")
        .arg("bash")
        .arg("-lc")
        .arg("rsync --version");
    let display = command.display();
    match runner.output(command) {
        Ok(output) if output.code == Some(0) => Ok(()),
        Ok(output) => Err(err(error_info::TOOL_RSYNC_NOT_FOUND)
            .with_command(display)
            .with_exit_code(output.code)
            .with_hint(
                first_non_empty(&output.stderr, &output.stdout)
                    .unwrap_or("请确认 WSL 默认发行版可启动，并且其中 rsync 在 PATH 中"),
            )),
        Err(source) => Err(err_with_source(error_info::TOOL_RSYNC_NOT_FOUND, source)
            .with_command(display)
            .with_hint("请通过 WSL、MSYS2、Git Bash 或 Cygwin 提供 rsync，并确认 rsync 可运行")),
    }
}

fn first_non_empty<'a>(first: &'a str, second: &'a str) -> Option<&'a str> {
    if !first.is_empty() {
        Some(first)
    } else if !second.is_empty() {
        Some(second)
    } else {
        None
    }
}

#[derive(Debug)]
struct RemoteTarget {
    host: String,
    port: u16,
}

impl RemoteTarget {
    fn from_config(config: &AppConfig) -> Self {
        Self {
            host: config.remote.host.clone(),
            port: config.remote.port,
        }
    }
}

fn check_remote_sh(runner: &impl ProcessRunner, remote: &RemoteTarget) -> Result<()> {
    check_remote_command(
        runner,
        RemoteCheckRequest::new(
            remote,
            "command -v sh",
            error_info::REMOTE_SSH_CONNECT_FAILED,
        ),
    )
}

fn check_remote_rsync(runner: &impl ProcessRunner, remote: &RemoteTarget) -> Result<()> {
    check_remote_command(
        runner,
        RemoteCheckRequest::new(
            remote,
            "command -v rsync",
            error_info::REMOTE_COMMAND_FAILED,
        ),
    )
}

fn check_remote_tar(runner: &impl ProcessRunner, remote: &RemoteTarget) -> Result<()> {
    check_remote_command(
        runner,
        RemoteCheckRequest::new(remote, "command -v tar", error_info::REMOTE_TAR_NOT_FOUND),
    )
}

fn check_remote_path_writable(
    runner: &impl ProcessRunner,
    remote: &RemoteTarget,
    path: &RemotePath,
) -> Result<()> {
    let command = format!(
        "mkdir -p {} && test -w {}",
        shell_quote(path.as_str()),
        shell_quote(path.as_str())
    );
    check_remote_command(
        runner,
        RemoteCheckRequest::new(remote, command, error_info::REMOTE_PATH_NOT_WRITABLE),
    )
}

struct RemoteCheckRequest<'a> {
    remote: &'a RemoteTarget,
    remote_command: String,
    error_info: ErrorInfo,
}

impl<'a> RemoteCheckRequest<'a> {
    fn new(
        remote: &'a RemoteTarget,
        remote_command: impl Into<String>,
        error_info: ErrorInfo,
    ) -> Self {
        Self {
            remote,
            remote_command: remote_command.into(),
            error_info,
        }
    }
}

fn check_remote_command(
    runner: &impl ProcessRunner,
    request: RemoteCheckRequest<'_>,
) -> Result<()> {
    let command = ssh_command(request.remote, &request.remote_command);
    let display = command.display();
    let output = runner.output(command).map_err(|source| {
        err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source)
            .with_remote(request.remote.host.clone())
            .with_command(display.clone())
    })?;
    if output.code == Some(0) {
        Ok(())
    } else {
        Err(err(request.error_info)
            .with_remote(request.remote.host.clone())
            .with_command(display)
            .with_hint(output.stderr))
    }
}

fn ssh_command(remote: &RemoteTarget, remote_command: &str) -> ProcessCommand {
    let remote_shell = format!("sh -c {}", shell_quote(&with_remote_path(remote_command)));
    ProcessCommand::new("ssh")
        .arg("-p")
        .arg(remote.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(remote.host.clone())
        .arg(remote_shell)
}

fn with_remote_path(command: &str) -> String {
    format!("PATH={}:$PATH; {}", shell_quote(REMOTE_BASE_PATH), command)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use crate::config::AppConfig;
    use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};

    use super::{run_doctor_with_exec, RemoteExecChecker};

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
                    stderr: String::new(),
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
            let output = self.outputs.borrow_mut().pop_front();
            match output {
                Some(output) => Ok(output),
                None => panic!("fake output missing"),
            }
        }
    }

    struct FakeExecChecker;

    impl RemoteExecChecker for FakeExecChecker {
        fn check_exec(&self) -> crate::error::Result<()> {
            Ok(())
        }
    }

    fn run_test_doctor(
        config: &AppConfig,
        runner: &impl ProcessRunner,
    ) -> crate::error::Result<super::DoctorReport> {
        run_doctor_with_exec(config, runner, &FakeExecChecker)
    }

    #[test]
    fn doctor_runs_expected_checks() {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let runner = FakeRunner::success(6);

        let report = run_test_doctor(&config, &runner);

        assert!(report.is_ok());
        let report = match report {
            Ok(report) => report,
            Err(error) => panic!("doctor should pass: {error}"),
        };
        let output = report.format_text();
        assert!(output.contains("config ok"));
        assert!(output.contains("remote.tar ok"));
        assert!(output.contains("remote.ssh_exec ok"));
        assert!(output.contains("remote.path.writable ok"));
        assert_eq!(runner.commands.borrow().len(), 6);
        assert!(runner
            .commands
            .borrow()
            .iter()
            .any(|command| command.contains("sh -c")));
    }

    #[test]
    fn doctor_rejects_unsafe_remote_path_before_running_commands() {
        let config = AppConfig::template("root@example.com", 22, "/root");
        let runner = FakeRunner::success(5);

        let report = run_test_doctor(&config, &runner);

        assert!(report.is_err());
        assert!(runner.commands.borrow().is_empty());
    }

    #[test]
    fn doctor_falls_back_to_wsl_rsync_in_auto_mode() {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let runner = FakeRunner::from_codes([
            Some(0),
            Some(1),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
        ]);

        let report = run_test_doctor(&config, &runner);

        assert!(report.is_ok());
        let commands = runner.commands.borrow();
        assert!(commands.iter().any(|command| command.starts_with("wsl ")));
    }
}
