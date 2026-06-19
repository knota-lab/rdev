use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::auth::run_auth_check;
use crate::cli::{
    AliasArgs, AliasCommand, AliasDeleteArgs, AliasSetArgs, Cli, Command, DaemonArgs, ExecArgs,
    InitArgs, RunArgs, SshArgs, SyncArgs,
};
use crate::command::{CommandExit, RunRequest, SshCommandBackend};
use crate::config::{AppConfig, CommandAliasConfig, SyncBackendKind, CONFIG_DIR_NAME};
use crate::daemon::{run_daemon_command, run_exec};
use crate::doctor::run_doctor;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::{RelativePath, RemotePath};
use crate::process::SystemProcessRunner;
use crate::sftp::SftpDeltaBackend;
use crate::sync::{RsyncSyncBackend, SyncBackend, SyncRequest};
use crate::tui::{run_tui, TuiRequest};
use crate::up::{run_up, UpRequest};

pub fn run(cli: Cli, cwd: &Path) -> Result<String> {
    match cli.command {
        Command::Init(args) => init(args, cwd),
        Command::Alias(args) => alias(args, cwd),
        Command::AuthCheck => auth_check(cwd),
        Command::Daemon(args) => daemon(args, cwd),
        Command::Doctor => doctor(cwd),
        Command::Exec(args) => exec(args, cwd),
        Command::Run(args) => run_command(args, cwd),
        Command::Sync(args) => sync(args, cwd),
        Command::Up(args) => up(args, cwd),
        Command::Status => status(cwd),
        Command::Stop => stop(cwd),
        Command::Ssh(args) => ssh(args, cwd),
    }
}

fn daemon(args: DaemonArgs, cwd: &Path) -> Result<String> {
    run_daemon_command(args, cwd)
}

fn alias(args: AliasArgs, cwd: &Path) -> Result<String> {
    match args.command {
        AliasCommand::List => alias_list(cwd),
        AliasCommand::Set(args) => alias_set(args, cwd),
        AliasCommand::Delete(args) => alias_delete(args, cwd),
    }
}

fn exec(args: ExecArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let resolved = resolve_command_alias(
        &config,
        AliasResolveRequest {
            command: args.command,
            explicit_dir: args.dir,
            params: args.params,
        },
    )?;
    if let Some(alias) = &resolved.alias {
        eprintln!("{}", alias_expansion_message(alias, &resolved));
    }
    run_exec(
        ExecArgs {
            command: resolved.command,
            dir: resolved.dir,
            summary: args.summary,
            params: Vec::new(),
        },
        cwd,
    )
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
    write_config(cwd, &config)?;
    Ok(format!("created {}", config_path.display()))
}

fn alias_list(cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    if config.commands.is_empty() {
        return Ok("no aliases configured".to_owned());
    }
    let mut lines = Vec::new();
    for (name, alias) in &config.commands {
        let dir = if alias.dir.trim().is_empty() {
            "."
        } else {
            alias.dir.as_str()
        };
        lines.push(format!("{name}\tdir={dir}\tcommand={}", alias.command));
    }
    Ok(lines.join("\n"))
}

fn alias_set(args: AliasSetArgs, cwd: &Path) -> Result<String> {
    validate_alias_name(&args.name)?;
    let command = args.command.join(" ").trim().to_owned();
    if command.is_empty() {
        return Err(err(error_info::CONFIG_INVALID).with_hint("alias command cannot be empty"));
    }
    if let Some(dir) = args.dir.as_deref() {
        RelativePath::parse(dir)?;
    }
    let mut config = AppConfig::load_from_dir(cwd)?;
    let existed = config.commands.contains_key(&args.name);
    config.commands.insert(
        args.name.clone(),
        CommandAliasConfig {
            command,
            dir: args
                .dir
                .as_ref()
                .map_or_else(String::new, |dir| dir.display().to_string()),
        },
    );
    write_config(cwd, &config)?;
    let action = if existed { "updated" } else { "created" };
    Ok(format!("alias {action}: {}", args.name))
}

fn alias_delete(args: AliasDeleteArgs, cwd: &Path) -> Result<String> {
    let mut config = AppConfig::load_from_dir(cwd)?;
    if config.commands.remove(&args.name).is_none() {
        return Err(
            err(error_info::CONFIG_INVALID).with_hint(format!("alias not found: {}", args.name))
        );
    }
    write_config(cwd, &config)?;
    Ok(format!("alias deleted: {}", args.name))
}

fn write_config(cwd: &Path, config: &AppConfig) -> Result<()> {
    let config_path = AppConfig::path_in_dir(cwd);
    let raw = toml::to_string_pretty(config)
        .map_err(|source| err_with_source(error_info::CONFIG_INVALID, source))?;
    fs::write(&config_path, raw).map_err(|source| {
        err_with_source(error_info::CONFIG_INVALID, source).with_path(config_path.display())
    })
}

fn validate_alias_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(err(error_info::CONFIG_INVALID).with_hint("alias name cannot be empty"));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(err(error_info::CONFIG_INVALID).with_hint(format!(
            "alias name may only contain letters, numbers, '-', '_' and '.': {name}"
        )));
    }
    Ok(())
}

fn doctor(cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let runner = SystemProcessRunner::default();
    let report = run_doctor(&config, &runner)?;
    Ok(report.format_text())
}

fn run_command(args: RunArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let resolved = resolve_command_alias(
        &config,
        AliasResolveRequest {
            command: args.command,
            explicit_dir: args.dir,
            params: args.params,
        },
    )?;
    let relative = parse_optional_dir(resolved.dir.as_deref())?;
    let runner = SystemProcessRunner::default();
    let backend = SshCommandBackend::new(&config, &runner);
    let output = backend.run(RunRequest {
        command: resolved.command.clone(),
        dir: relative,
        sync_before_run: !args.no_sync,
    })?;
    let formatted = format_command_output(output);
    if let Some(alias) = &resolved.alias {
        let message = alias_expansion_message(alias, &resolved);
        if formatted.is_empty() {
            Ok(message)
        } else {
            Ok(format!("{message}\n{formatted}"))
        }
    } else {
        Ok(formatted)
    }
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
    if args.tui {
        run_tui(
            &config,
            TuiRequest {
                project_root: cwd.to_path_buf(),
                poll: args.poll,
            },
        )?;
        return Ok(String::new());
    }
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

fn status(cwd: &Path) -> Result<String> {
    Ok(crate::up::up_status(cwd)?.format_text())
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCommand {
    alias: Option<String>,
    command: String,
    dir: Option<PathBuf>,
}

struct AliasResolveRequest {
    command: String,
    explicit_dir: Option<PathBuf>,
    params: Vec<String>,
}

fn resolve_command_alias(
    config: &AppConfig,
    request: AliasResolveRequest,
) -> Result<ResolvedCommand> {
    let Some(alias) = config.commands.get(&request.command) else {
        if !request.params.is_empty() {
            return Err(err(error_info::CONFIG_INVALID)
                .with_hint("alias parameters require a configured command alias"));
        }
        return Ok(ResolvedCommand {
            alias: None,
            command: request.command,
            dir: request.explicit_dir,
        });
    };
    let params = parse_alias_params(request.params)?;
    let expanded = expand_alias_command(&alias.command, &params)?;
    let alias_dir = if alias.dir.trim().is_empty() {
        None
    } else {
        Some(PathBuf::from(alias.dir.as_str()))
    };
    Ok(ResolvedCommand {
        alias: Some(request.command),
        command: expanded,
        dir: request.explicit_dir.or(alias_dir),
    })
}

fn parse_alias_params(params: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    for param in params {
        let Some((key, value)) = param.split_once('=') else {
            return Err(err(error_info::CONFIG_INVALID)
                .with_hint(format!("alias parameter must be key=value: {param}")));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(err(error_info::CONFIG_INVALID)
                .with_hint(format!("alias parameter key is empty: {param}")));
        }
        parsed.insert(key.to_owned(), value.to_owned());
    }
    Ok(parsed)
}

fn expand_alias_command(command: &str, params: &BTreeMap<String, String>) -> Result<String> {
    let mut expanded = command.to_owned();
    for (key, value) in params {
        expanded = expanded.replace(&format!("{{{key}}}"), value);
    }
    let missing = missing_alias_params(&expanded);
    if !missing.is_empty() {
        return Err(err(error_info::CONFIG_INVALID).with_hint(format!(
            "missing alias parameter(s): {}",
            missing.join(", ")
        )));
    }
    Ok(expanded)
}

fn missing_alias_params(command: &str) -> Vec<String> {
    let mut missing = Vec::new();
    let mut rest = command;
    while let Some(start) = rest.find('{') {
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('}') else {
            break;
        };
        let key = &after_start[..end];
        if !key.is_empty()
            && key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
            && !missing.iter().any(|item| item == key)
        {
            missing.push(key.to_owned());
        }
        rest = &after_start[end + 1..];
    }
    missing
}

fn alias_expansion_message(alias: &str, resolved: &ResolvedCommand) -> String {
    let dir = resolved
        .dir
        .as_ref()
        .map_or_else(|| ".".to_owned(), |dir| dir.display().to_string());
    format!(
        "[rdev] alias {alias} -> dir={dir} command={}",
        resolved.command
    )
}

fn resolve_local_root(project_root: &Path, local_path: &Path) -> std::path::PathBuf {
    if local_path.is_absolute() {
        local_path.to_path_buf()
    } else {
        project_root.join(local_path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use crate::cli::{AliasDeleteArgs, AliasSetArgs};
    use crate::config::{AppConfig, CommandAliasConfig};

    use super::{
        alias_delete, alias_set, resolve_command_alias, AliasResolveRequest, ResolvedCommand,
    };

    #[test]
    fn resolves_command_alias_with_dir_and_params() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.commands.insert(
            "l2-session".to_owned(),
            CommandAliasConfig {
                dir: "knota-fold".to_owned(),
                command: "cargo run -- task l2_process_session session_id:{session_id}".to_owned(),
            },
        );

        let resolved = match resolve_command_alias(
            &config,
            AliasResolveRequest {
                command: "l2-session".to_owned(),
                explicit_dir: None,
                params: vec!["session_id=26".to_owned()],
            },
        ) {
            Ok(resolved) => resolved,
            Err(error) => panic!("alias should resolve: {error}"),
        };

        assert_eq!(
            resolved,
            ResolvedCommand {
                alias: Some("l2-session".to_owned()),
                command: "cargo run -- task l2_process_session session_id:26".to_owned(),
                dir: Some(PathBuf::from("knota-fold")),
            }
        );
    }

    #[test]
    fn explicit_dir_overrides_alias_dir() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.commands.insert(
            "build".to_owned(),
            CommandAliasConfig {
                dir: "backend".to_owned(),
                command: "cargo build".to_owned(),
            },
        );

        let resolved = match resolve_command_alias(
            &config,
            AliasResolveRequest {
                command: "build".to_owned(),
                explicit_dir: Some(PathBuf::from("frontend")),
                params: Vec::new(),
            },
        ) {
            Ok(resolved) => resolved,
            Err(error) => panic!("alias should resolve: {error}"),
        };

        assert_eq!(
            resolved,
            ResolvedCommand {
                alias: Some("build".to_owned()),
                command: "cargo build".to_owned(),
                dir: Some(PathBuf::from("frontend")),
            }
        );
    }

    #[test]
    fn missing_alias_param_is_config_error() {
        let mut config = AppConfig::template("root@example.com", 22, "/root/project");
        config.commands.insert(
            "task".to_owned(),
            CommandAliasConfig {
                dir: String::new(),
                command: "cargo run -- task {name}".to_owned(),
            },
        );

        let error = match resolve_command_alias(
            &config,
            AliasResolveRequest {
                command: "task".to_owned(),
                explicit_dir: None,
                params: Vec::new(),
            },
        ) {
            Ok(_) => panic!("missing param should fail"),
            Err(error) => error,
        };

        assert_eq!(error.info.code, "config.invalid");
    }

    #[test]
    fn alias_set_creates_and_updates_config_alias() {
        let root = temp_project("rdev-alias-set-test");
        write_test_config(&root);

        let created = alias_set(
            AliasSetArgs {
                name: "backend-lint".to_owned(),
                dir: Some(PathBuf::from("backend")),
                command: vec!["cargo clippy --all-features -- -D warnings".to_owned()],
            },
            &root,
        );

        let created = match created {
            Ok(message) => message,
            Err(error) => panic!("alias set should create: {error}"),
        };
        assert_eq!(created, "alias created: backend-lint");
        let config = load_test_config(&root);
        let alias = match config.commands.get("backend-lint") {
            Some(alias) => alias,
            None => panic!("alias should exist"),
        };
        assert_eq!(alias.dir, "backend");
        assert_eq!(alias.command, "cargo clippy --all-features -- -D warnings");

        let updated = alias_set(
            AliasSetArgs {
                name: "backend-lint".to_owned(),
                dir: Some(PathBuf::from("api")),
                command: vec!["cargo test".to_owned()],
            },
            &root,
        );

        let updated = match updated {
            Ok(message) => message,
            Err(error) => panic!("alias set should update: {error}"),
        };
        assert_eq!(updated, "alias updated: backend-lint");
        let config = load_test_config(&root);
        let alias = match config.commands.get("backend-lint") {
            Some(alias) => alias,
            None => panic!("alias should exist"),
        };
        assert_eq!(alias.dir, "api");
        assert_eq!(alias.command, "cargo test");
        let _cleanup = fs::remove_dir_all(root);
    }

    #[test]
    fn alias_delete_removes_config_alias() {
        let root = temp_project("rdev-alias-delete-test");
        write_test_config(&root);
        let set = alias_set(
            AliasSetArgs {
                name: "build".to_owned(),
                dir: None,
                command: vec!["cargo build".to_owned()],
            },
            &root,
        );
        assert!(set.is_ok());

        let deleted = alias_delete(
            AliasDeleteArgs {
                name: "build".to_owned(),
            },
            &root,
        );

        let deleted = match deleted {
            Ok(message) => message,
            Err(error) => panic!("alias delete should succeed: {error}"),
        };
        assert_eq!(deleted, "alias deleted: build");
        let config = load_test_config(&root);
        assert!(!config.commands.contains_key("build"));
        let _cleanup = fs::remove_dir_all(root);
    }

    fn temp_project(prefix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
        let _cleanup = fs::remove_dir_all(&root);
        if let Err(error) = fs::create_dir_all(root.join(".rdev")) {
            panic!("create temp project: {error}");
        }
        root
    }

    fn write_test_config(root: &std::path::Path) {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let raw = match toml::to_string_pretty(&config) {
            Ok(raw) => raw,
            Err(error) => panic!("serialize config: {error}"),
        };
        if let Err(error) = fs::write(root.join(".rdev").join("config.toml"), raw) {
            panic!("write config: {error}");
        }
    }

    fn load_test_config(root: &std::path::Path) -> AppConfig {
        match AppConfig::load_from_dir(root) {
            Ok(config) => config,
            Err(error) => panic!("load config: {error}"),
        }
    }
}
