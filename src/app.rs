use std::fs;
use std::path::Path;

use crate::auth::run_auth_check;
use crate::cli::{Cli, Command, InitArgs, RunArgs, SshArgs, SyncArgs};
use crate::command::{CommandExit, RunRequest, SshCommandBackend};
use crate::config::{AppConfig, SyncBackendKind, CONFIG_DIR_NAME};
use crate::doctor::run_doctor;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::{RelativePath, RemotePath};
use crate::process::SystemProcessRunner;
use crate::sftp::SftpDeltaBackend;
use crate::sync::{RsyncSyncBackend, SyncBackend, SyncRequest};
use crate::up::{run_up, UpRequest};

pub fn run(cli: Cli, cwd: &Path) -> Result<String> {
    match cli.command {
        Command::Init(args) => init(args, cwd),
        Command::AuthCheck => auth_check(cwd),
        Command::Doctor => doctor(cwd),
        Command::Run(args) => run_command(args, cwd),
        Command::Sync(args) => sync(args, cwd),
        Command::Up(args) => up(args, cwd),
        Command::Stop => stop(cwd),
        Command::Ssh(args) => ssh(args, cwd),
    }
}

fn auth_check(cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    run_auth_check(&config)
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

    let config_dir = cwd.join(CONFIG_DIR_NAME);
    fs::create_dir_all(&config_dir)
        .map_err(|source| err_with_source(error_info::CONFIG_INVALID, source))?;
    let config_path = AppConfig::path_in_dir(cwd);
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

fn sync(args: SyncArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let runner = SystemProcessRunner::default();
    let rsync_backend = RsyncSyncBackend::new(&config, &runner);
    let ssh_backend = SftpDeltaBackend::new(&config);
    let backend = sync_backend(&config, &rsync_backend, &ssh_backend);
    let report = backend.sync_full(SyncRequest {
        dry_run: args.dry_run,
        delete: config.sync.delete && !args.no_delete,
        project_root: resolve_local_root(cwd, &config.sync.local_path),
        cancelled: None,
    })?;
    Ok(report.format_text())
}

fn sync_backend<'a>(
    config: &AppConfig,
    rsync: &'a dyn SyncBackend,
    ssh: &'a dyn SyncBackend,
) -> &'a dyn SyncBackend {
    match config.sync.backend {
        SyncBackendKind::Rsync => rsync,
        SyncBackendKind::Ssh | SyncBackendKind::Auto => ssh,
    }
}

fn up(args: crate::cli::UpArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let runner = SystemProcessRunner::default();
    run_up(
        &config,
        &runner,
        UpRequest {
            project_root: cwd.to_path_buf(),
            initial_sync: !args.no_initial_sync,
            poll: args.poll,
        },
    )?;
    Ok(String::new())
}

fn stop(cwd: &Path) -> Result<String> {
    crate::up::request_stop(cwd)?;
    Ok("stop requested".to_owned())
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

fn resolve_local_root(project_root: &Path, local_path: &Path) -> std::path::PathBuf {
    if local_path.is_absolute() {
        local_path.to_path_buf()
    } else {
        project_root.join(local_path)
    }
}
