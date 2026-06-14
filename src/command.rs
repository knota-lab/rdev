use crate::config::AppConfig;
use crate::error::{err, err_with_source, RdevError, Result};
use crate::error_info;
use crate::path::{RelativePath, RemotePath};
use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub command: String,
    pub dir: RelativePath,
    pub sync_before_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExit {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub struct SshCommandBackend<'a, R> {
    config: &'a AppConfig,
    runner: &'a R,
}

impl<'a, R> SshCommandBackend<'a, R>
where
    R: ProcessRunner,
{
    pub fn new(config: &'a AppConfig, runner: &'a R) -> Self {
        Self { config, runner }
    }

    pub fn run(&self, request: RunRequest) -> Result<CommandExit> {
        let remote_root = RemotePath::parse(self.config.remote.path.as_str())?;
        let remote_dir = remote_root.join_relative(&request.dir);
        let command = ssh_run_command(self.config, &remote_dir, &request.command);
        let display = command.display();
        let output = self.runner.output(command).map_err(|source| {
            err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source)
                .with_remote(self.config.remote.host.clone())
                .with_command(display.clone())
        })?;

        if output.code == Some(0) {
            Ok(output.into())
        } else if output.code == Some(255) {
            Err(ssh_failed_error(
                output,
                display,
                self.config.remote.host.clone(),
            ))
        } else {
            Err(remote_command_error(
                output,
                display,
                self.config.remote.host.clone(),
            ))
        }
    }
}

fn ssh_failed_error(output: ProcessOutput, command: String, remote: String) -> RdevError {
    let mut error = err(error_info::REMOTE_SSH_CONNECT_FAILED)
        .with_remote(remote)
        .with_command(command)
        .with_exit_code(output.code);
    if !output.stderr.is_empty() {
        error = error.with_hint(output.stderr);
    }
    error
}

fn remote_command_error(output: ProcessOutput, command: String, remote: String) -> RdevError {
    let mut error = err(error_info::REMOTE_COMMAND_FAILED)
        .with_remote(remote)
        .with_command(command)
        .with_exit_code(output.code);
    if !output.stderr.is_empty() {
        error = error.with_hint(output.stderr);
    }
    error
}

fn ssh_run_command(
    config: &AppConfig,
    remote_dir: &RemotePath,
    user_command: &str,
) -> ProcessCommand {
    let remote_command = format!(
        "cd {} && {}",
        shell_quote(remote_dir.as_str()),
        user_command
    );
    let remote_shell = format!("sh -lc {}", shell_quote(&remote_command));
    ProcessCommand::new("ssh")
        .arg("-p")
        .arg(config.remote.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg(config.remote.host.clone())
        .arg(remote_shell)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl From<ProcessOutput> for CommandExit {
    fn from(output: ProcessOutput) -> Self {
        Self {
            code: output.code,
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::config::AppConfig;
    use crate::path::RelativePath;
    use crate::process::{ProcessCommand, ProcessOutput, ProcessRunner};

    use super::{RunRequest, SshCommandBackend};

    #[derive(Debug)]
    struct FakeRunner {
        output: ProcessOutput,
        command: RefCell<Option<String>>,
    }

    impl ProcessRunner for FakeRunner {
        fn output(&self, command: ProcessCommand) -> std::io::Result<ProcessOutput> {
            self.command.replace(Some(command.display()));
            Ok(self.output.clone())
        }
    }

    #[test]
    fn builds_ssh_run_command_in_remote_dir() {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let runner = FakeRunner {
            output: ProcessOutput {
                code: Some(0),
                stdout: "ok".to_owned(),
                stderr: String::new(),
            },
            command: RefCell::new(None),
        };
        let backend = SshCommandBackend::new(&config, &runner);
        let request = RunRequest {
            command: "cargo test".to_owned(),
            dir: match RelativePath::parse("backend") {
                Ok(path) => path,
                Err(error) => panic!("dir should parse: {error}"),
            },
            sync_before_run: false,
        };

        let result = backend.run(request);

        assert!(result.is_ok());
        let command = runner.command.borrow();
        let command = match command.as_ref() {
            Some(command) => command,
            None => panic!("command should be captured"),
        };
        assert!(command.contains("root@example.com"));
        assert!(command.contains("sh -lc"));
        assert!(command.contains("/root/project/backend"));
        assert!(command.contains("cargo test"));
    }

    #[test]
    fn maps_ssh_255_to_connect_failed() {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let runner = FakeRunner {
            output: ProcessOutput {
                code: Some(255),
                stdout: String::new(),
                stderr: "connection failed".to_owned(),
            },
            command: RefCell::new(None),
        };
        let backend = SshCommandBackend::new(&config, &runner);
        let request = RunRequest {
            command: "pwd".to_owned(),
            dir: match RelativePath::parse(".") {
                Ok(path) => path,
                Err(error) => panic!("dir should parse: {error}"),
            },
            sync_before_run: false,
        };

        let result = backend.run(request);

        assert!(result.is_err());
        let error = match result {
            Ok(_) => panic!("run should fail"),
            Err(error) => error,
        };
        assert_eq!(error.info.code, "remote.ssh_connect_failed");
    }
}
