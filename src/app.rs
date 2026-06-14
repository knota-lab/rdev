use std::fs;
use std::path::Path;

use crate::cli::{Cli, Command, InitArgs, RunArgs, SshArgs};
use crate::command::{CommandExit, RunRequest, SshCommandBackend};
use crate::config::{AppConfig, CONFIG_FILE_NAME};
use crate::doctor::run_doctor;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::{RelativePath, RemotePath};
use crate::process::SystemProcessRunner;

pub fn run(cli: Cli, cwd: &Path) -> Result<String> {
    match cli.command {
        Command::Init(args) => init(args, cwd),
        Command::Doctor => doctor(cwd),
        Command::Run(args) => run_command(args, cwd),
        Command::Sync(_) => Ok("sync is not implemented yet".to_owned()),
        Command::Up(_) => Ok("up is not implemented yet".to_owned()),
        Command::Ssh(args) => ssh(args, cwd),
    }
}

fn init(args: InitArgs, cwd: &Path) -> Result<String> {
    let host = args.host.ok_or_else(|| {
        err(error_info::CONFIG_INVALID)
            .with_hint("请使用 --host 指定远端主机，例如 root@example.com")
    })?;
    let remote_path = args.path.ok_or_else(|| {
        err(error_info::CONFIG_INVALID)
            .with_hint("请使用 --path 指定远端项目目录，例如 /root/project")
    })?;
    RemotePath::parse(remote_path.as_str())?;

    let config_path = cwd.join(CONFIG_FILE_NAME);
    let config = AppConfig::template(&host, args.port, &remote_path);
    let raw = toml::to_string_pretty(&config)
        .map_err(|source| err_with_source(error_info::CONFIG_INVALID, source))?;
    fs::write(&config_path, raw)
        .map_err(|source| err_with_source(error_info::CONFIG_INVALID, source))?;
    Ok(format!("created {}", config_path.display()))
}

fn doctor(cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let runner = SystemProcessRunner::default();
    let report = run_doctor(&config, &runner)?;
    Ok(report.format_text())
}

fn run_command(args: RunArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let relative = parse_optional_dir(args.dir.as_deref())?;
    let runner = SystemProcessRunner::default();
    let backend = SshCommandBackend::new(&config, &runner);
    let output = backend.run(RunRequest {
        command: args.command,
        dir: relative,
        sync_before_run: !args.no_sync,
    })?;
    Ok(format_command_output(output))
}

fn format_command_output(output: CommandExit) -> String {
    match (output.stdout.is_empty(), output.stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => output.stdout,
        (true, false) => output.stderr,
        (false, false) => format!("{}\n{}", output.stdout, output.stderr),
    }
}

fn ssh(args: SshArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let remote_root = RemotePath::parse(config.remote.path)?;
    let relative = parse_optional_dir(args.dir.as_deref())?;
    let remote_dir = remote_root.join_relative(&relative);
    Ok(format!("would open ssh in {remote_dir}"))
}

fn parse_optional_dir(path: Option<&Path>) -> Result<RelativePath> {
    match path {
        Some(path) => RelativePath::parse(path),
        None => RelativePath::parse("."),
    }
}
