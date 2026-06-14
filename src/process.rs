use std::ffi::OsStr;
use std::io;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub trait ProcessRunner {
    fn output(&self, command: ProcessCommand) -> io::Result<ProcessOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCommand {
    program: String,
    args: Vec<String>,
}

impl ProcessCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().map(|arg| shellish_quote(arg)));
        parts.join(" ")
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn args_slice(&self) -> &[String] {
        &self.args
    }
}

#[derive(Debug, Default)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn output(&self, command: ProcessCommand) -> io::Result<ProcessOutput> {
        let output = Command::new(command.program())
            .args(command.args_slice().iter().map(OsStr::new))
            .output()?;

        Ok(ProcessOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn shellish_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '\\'))
    {
        value.to_owned()
    } else {
        format!("{value:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessCommand;

    #[test]
    fn displays_command_with_quoted_args() {
        let command = ProcessCommand::new("ssh")
            .arg("root@example.com")
            .arg("echo hello");

        assert_eq!(command.display(), "ssh \"root@example.com\" \"echo hello\"");
    }
}
