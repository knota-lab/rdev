use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::auth::run_auth_check;
use crate::cli::{
    AliasArgs, AliasCommand, AliasDeleteArgs, AliasSetArgs, Cli, Command, DaemonArgs, ExecArgs,
    InitArgs, RunArgs, ServiceArgs, ServiceCommand, ServiceLogsArgs, ServiceSetArgs,
    ServiceStartArgs, ServiceStatusArgs, ServiceStopArgs, ServiceWaitArgs, SshArgs, SyncArgs,
    WhyIgnoreArgs,
};
use crate::command::{CommandExit, RunRequest, SshCommandBackend};
use crate::config::{
    AppConfig, CommandAliasConfig, ServiceConfig, SyncBackendKind, CONFIG_DIR_NAME,
};
use crate::daemon::{run_daemon_command, run_exec};
use crate::doctor::run_doctor;
use crate::error::{err, err_with_source, Result};
use crate::error_info;
use crate::path::{RelativePath, RemotePath, SyncExclusionExplanation, SyncExclusionReason};
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
        Command::WhyIgnore(args) => why_ignore(args, cwd),
        Command::Daemon(args) => daemon(args, cwd),
        Command::Doctor => doctor(cwd),
        Command::Exec(args) => exec(args, cwd),
        Command::Run(args) => run_command(args, cwd),
        Command::Service(args) => service(args, cwd),
        Command::Sync(args) => sync(args, cwd),
        Command::Up(args) => up(args, cwd),
        Command::Status => status(cwd),
        Command::Stop => stop(cwd),
        Command::Ssh(args) => ssh(args, cwd),
    }
}

fn why_ignore(args: WhyIgnoreArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let local_root = resolve_local_root(cwd, &config.sync.local_path);
    let path = if args.path.is_absolute() {
        args.path
    } else {
        local_root.join(args.path)
    };
    let explanation = crate::path::explain_sync_exclusion(&path, &local_root, &config.sync.exclude);
    Ok(format_sync_exclusion(explanation))
}

fn format_sync_exclusion(explanation: SyncExclusionExplanation) -> String {
    let relative = explanation.relative_path.as_deref().unwrap_or("<outside>");
    let status = if explanation.excluded {
        "ignored"
    } else {
        "included"
    };
    let reason = match explanation.reason {
        SyncExclusionReason::OutsideLocalRoot => {
            "path is outside configured local sync root".to_owned()
        }
        SyncExclusionReason::ProjectRoot => "project root is never ignored".to_owned(),
        SyncExclusionReason::NoMatchingRule => "no matching exclude rule".to_owned(),
        SyncExclusionReason::MatchedRule {
            rule,
            include,
            pattern,
        } => {
            let kind = if include { "include" } else { "exclude" };
            format!("matched {kind} rule `{rule}` pattern=`{pattern}`")
        }
    };
    format!("path={relative}\nstatus={status}\nreason={reason}")
}

fn daemon(args: DaemonArgs, cwd: &Path) -> Result<String> {
    run_daemon_command(args, cwd)
}

fn service(args: ServiceArgs, cwd: &Path) -> Result<String> {
    match args.command {
        ServiceCommand::List => service_list(cwd),
        ServiceCommand::Set(args) => service_set(args, cwd),
        ServiceCommand::Start(args) => service_start(args, cwd),
        ServiceCommand::Wait(args) => service_wait(args, cwd),
        ServiceCommand::Status(args) => service_status(args, cwd),
        ServiceCommand::Logs(args) => service_logs(args, cwd),
        ServiceCommand::Stop(args) => service_stop(args, cwd),
    }
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

fn service_list(cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    if config.services.is_empty() {
        return Ok("no services configured".to_owned());
    }
    let mut lines = Vec::new();
    for (name, service) in &config.services {
        let url = if service.url.trim().is_empty() {
            "-"
        } else {
            service.url.as_str()
        };
        lines.push(format!(
            "{name}\tdir={}\tready_pattern={}\turl={url}\tcommand={}",
            service_dir_label(service),
            service.ready_pattern,
            service.command
        ));
    }
    Ok(lines.join("\n"))
}

fn service_set(args: ServiceSetArgs, cwd: &Path) -> Result<String> {
    validate_service_name(&args.name)?;
    let command = args.command.join(" ").trim().to_owned();
    if command.is_empty() {
        return Err(err(error_info::CONFIG_INVALID).with_hint("service command cannot be empty"));
    }
    if args.ready.trim().is_empty() {
        return Err(
            err(error_info::CONFIG_INVALID).with_hint("service ready pattern cannot be empty")
        );
    }
    if let Some(dir) = args.dir.as_deref() {
        RelativePath::parse(dir)?;
    }
    let mut config = AppConfig::load_from_dir(cwd)?;
    let existed = config.services.contains_key(&args.name);
    config.services.insert(
        args.name.clone(),
        ServiceConfig {
            command,
            dir: args
                .dir
                .as_ref()
                .map_or_else(String::new, |dir| dir.display().to_string()),
            ready_pattern: args.ready,
            url: args.url,
        },
    );
    write_config(cwd, &config)?;
    let action = if existed { "updated" } else { "created" };
    Ok(format!("service {action}: {}", args.name))
}

fn service_start(args: ServiceStartArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let service = configured_service(&config, &args.name)?;
    validate_service(&args.name, &service)?;
    run_exec(
        ExecArgs {
            command: build_service_start_command(ServiceCommandBuild {
                config: &config,
                name: &args.name,
                service: &service,
                value: args.timeout,
            })?,
            dir: None,
            summary: false,
            params: Vec::new(),
        },
        cwd,
    )
}

fn service_wait(args: ServiceWaitArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let service = configured_service(&config, &args.name)?;
    validate_service(&args.name, &service)?;
    run_exec(
        ExecArgs {
            command: build_service_wait_command(ServiceCommandBuild {
                config: &config,
                name: &args.name,
                service: &service,
                value: args.timeout,
            })?,
            dir: None,
            summary: false,
            params: Vec::new(),
        },
        cwd,
    )
}

fn service_status(args: ServiceStatusArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let service = configured_service(&config, &args.name)?;
    validate_service(&args.name, &service)?;
    run_exec(
        ExecArgs {
            command: build_service_status_command(ServiceCommandBuild {
                config: &config,
                name: &args.name,
                service: &service,
                value: 0,
            })?,
            dir: None,
            summary: false,
            params: Vec::new(),
        },
        cwd,
    )
}

fn service_logs(args: ServiceLogsArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let service = configured_service(&config, &args.name)?;
    validate_service(&args.name, &service)?;
    run_exec(
        ExecArgs {
            command: build_service_logs_command(ServiceCommandBuild {
                config: &config,
                name: &args.name,
                service: &service,
                value: u64::from(args.lines),
            })?,
            dir: None,
            summary: false,
            params: Vec::new(),
        },
        cwd,
    )
}

fn service_stop(args: ServiceStopArgs, cwd: &Path) -> Result<String> {
    let config = AppConfig::load_from_dir(cwd)?;
    let service = configured_service(&config, &args.name)?;
    validate_service(&args.name, &service)?;
    run_exec(
        ExecArgs {
            command: build_service_stop_command(ServiceCommandBuild {
                config: &config,
                name: &args.name,
                service: &service,
                value: 0,
            })?,
            dir: None,
            summary: false,
            params: Vec::new(),
        },
        cwd,
    )
}

fn validate_service(name: &str, service: &ServiceConfig) -> Result<()> {
    if service.command.trim().is_empty() {
        return Err(
            err(error_info::CONFIG_INVALID).with_hint(format!("service command is empty: {name}"))
        );
    }
    if service.ready_pattern.trim().is_empty() {
        return Err(err(error_info::CONFIG_INVALID)
            .with_hint(format!("service ready_pattern is empty: {name}")));
    }
    if !service.dir.trim().is_empty() {
        RelativePath::parse(&service.dir)?;
    }
    Ok(())
}

fn service_dir_label(service: &ServiceConfig) -> &str {
    if service.dir.trim().is_empty() {
        "."
    } else {
        service.dir.as_str()
    }
}

fn configured_service(config: &AppConfig, name: &str) -> Result<ServiceConfig> {
    config
        .services
        .get(name)
        .ok_or_else(|| {
            err(error_info::CONFIG_INVALID).with_hint(format!("service not found: {name}"))
        })
        .cloned()
}

struct ServiceCommandBuild<'a> {
    config: &'a AppConfig,
    name: &'a str,
    service: &'a ServiceConfig,
    value: u64,
}

fn build_service_start_command(build: ServiceCommandBuild<'_>) -> Result<String> {
    let paths = service_paths(build.config, build.name, build.service)?;
    let ready_line = service_ready_line(build.name, build.service);
    let wait_script = service_wait_script();
    Ok(format!(
        r#"service_name={name}
state_dir={state_dir}
work_dir={work_dir}
pid_file="$state_dir/pid"
log_file="$state_dir/output.log"
status_file="$state_dir/status"
ready_file="$state_dir/ready"
pid=""
started_here=0
cleanup_start() {{
  if [ "$started_here" = "1" ] && [ -n "$pid" ] && [ ! -f "$ready_file" ]; then
    kill -INT -"$pid" 2>/dev/null || kill -INT "$pid" 2>/dev/null || true
    sleep 1
    kill -TERM -"$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
    sleep 1
    kill -KILL -"$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
    rm -f "$pid_file"
    echo "stopped" > "$status_file"
  fi
}}
trap cleanup_start INT TERM HUP
mkdir -p "$state_dir"
if [ -f "$pid_file" ]; then
  pid=$(cat "$pid_file" 2>/dev/null || true)
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    echo "[service] $service_name already running pid=$pid"
    echo "[service] log=$log_file"
    if [ -f "$ready_file" ]; then
      echo {ready_line}
      exit 0
    fi
  else
    pid=""
  fi
fi
if [ -z "$pid" ]; then
  rm -f "$pid_file" "$ready_file"
  : > "$log_file"
  if command -v setsid >/dev/null 2>&1; then
    setsid sh -lc {command} </dev/null >>"$log_file" 2>&1 &
  else
    nohup sh -lc {command} </dev/null >>"$log_file" 2>&1 &
  fi
  pid=$!
  started_here=1
  echo "$pid" > "$pid_file"
  echo "running" > "$status_file"
  echo "[service] $service_name started pid=$pid"
  echo "[service] log=$log_file"
fi
ready_pattern={ready_pattern}
ready_line={ready_line}
wait_timeout={timeout}
deadline=$(( $(date +%s) + wait_timeout ))
started_at=$(date +%s)
last_heartbeat=$started_at
offset=0
{wait_script}"#,
        name = shell_quote(build.name),
        state_dir = shell_quote(&paths.state_dir),
        work_dir = shell_quote(&paths.work_dir),
        command = shell_quote(&format!(
            "cd {} && exec sh -lc {}",
            shell_quote(&paths.work_dir),
            shell_quote(&build.service.command)
        )),
        ready_pattern = shell_quote(&build.service.ready_pattern),
        ready_line = shell_quote(&ready_line),
        timeout = build.value,
        wait_script = wait_script,
    ))
}

fn build_service_wait_command(build: ServiceCommandBuild<'_>) -> Result<String> {
    let paths = service_paths(build.config, build.name, build.service)?;
    let ready_line = service_ready_line(build.name, build.service);
    let wait_script = service_wait_script();
    Ok(format!(
        r#"service_name={name}
state_dir={state_dir}
pid_file="$state_dir/pid"
log_file="$state_dir/output.log"
status_file="$state_dir/status"
ready_file="$state_dir/ready"
if [ ! -f "$pid_file" ]; then
  echo "[service] $service_name not running" >&2
  exit 66
fi
pid=$(cat "$pid_file" 2>/dev/null || true)
if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
  echo "[service] $service_name not running" >&2
  echo "stopped" > "$status_file"
  exit 66
fi
echo "[service] waiting $service_name pid=$pid"
echo "[service] log=$log_file"
if [ -f "$ready_file" ]; then
  echo {ready_line}
  exit 0
fi
ready_pattern={ready_pattern}
ready_line={ready_line}
wait_timeout={timeout}
deadline=$(( $(date +%s) + wait_timeout ))
started_at=$(date +%s)
last_heartbeat=$started_at
offset=0
{wait_script}"#,
        name = shell_quote(build.name),
        state_dir = shell_quote(&paths.state_dir),
        ready_pattern = shell_quote(&build.service.ready_pattern),
        ready_line = shell_quote(&ready_line),
        timeout = build.value,
        wait_script = wait_script,
    ))
}

fn service_wait_script() -> &'static str {
    r#"while :; do
  if [ -f "$log_file" ]; then
    size=$(wc -c < "$log_file" 2>/dev/null || echo 0)
    if [ "$size" -gt "$offset" ] 2>/dev/null; then
      dd if="$log_file" bs=1 skip="$offset" count=$((size - offset)) 2>/dev/null || true
      offset=$size
    fi
    if grep -F -- "$ready_pattern" "$log_file" >/dev/null 2>&1; then
      date > "$ready_file"
      echo "$ready_line"
      echo "[service] logs: rdev service logs $service_name"
      echo "[service] remote_log=$log_file"
      exit 0
    fi
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "exited" > "$status_file"
    wait "$pid"
    code=$?
    echo "[service] $service_name exited before ready code=$code" >&2
    exit "$code"
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "[service] $service_name ready timeout after ${wait_timeout}s" >&2
    echo "[service] $service_name still running pid=$pid" >&2
    echo "[service] inspect with: rdev service status $service_name" >&2
    echo "[service] logs with: rdev service logs $service_name" >&2
    echo "[service] stop with: rdev service stop $service_name" >&2
    exit 124
  fi
  now=$(date +%s)
  if [ $((now - last_heartbeat)) -ge 10 ]; then
    elapsed=$((now - started_at))
    echo "[service] waiting elapsed=${elapsed}s timeout=${wait_timeout}s pid=$pid log=$log_file" >&2
    last_heartbeat=$now
  fi
  sleep 1
done"#
}

fn build_service_status_command(build: ServiceCommandBuild<'_>) -> Result<String> {
    let paths = service_paths(build.config, build.name, build.service)?;
    Ok(format!(
        r#"service_name={name}
state_dir={state_dir}
pid_file="$state_dir/pid"
log_file="$state_dir/output.log"
ready_file="$state_dir/ready"
status="stopped"
pid=""
if [ -f "$pid_file" ]; then
  pid=$(cat "$pid_file" 2>/dev/null || true)
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    status="running"
  fi
fi
ready="false"
if [ -f "$ready_file" ]; then ready="true"; fi
if [ "$status" = "stopped" ] && [ "$ready" = "true" ]; then
  status="stale_ready"
fi
echo "service=$service_name"
echo "status=$status"
echo "pid=$pid"
echo "ready=$ready"
echo "remote_log=$log_file"
echo "logs_command=rdev service logs $service_name"
echo "wait_command=rdev service wait $service_name"
echo "stop_command=rdev service stop $service_name"
printf 'url=%s\n' {url}"#,
        name = shell_quote(build.name),
        state_dir = shell_quote(&paths.state_dir),
        url = shell_quote(&build.service.url),
    ))
}

fn build_service_logs_command(build: ServiceCommandBuild<'_>) -> Result<String> {
    let paths = service_paths(build.config, build.name, build.service)?;
    Ok(format!(
        r#"log_file={log_file}
if [ ! -f "$log_file" ]; then
  echo "[service] log not found: $log_file" >&2
  exit 66
fi
tail -n {lines} "$log_file""#,
        log_file = shell_quote(&paths.log_file),
        lines = build.value,
    ))
}

fn build_service_stop_command(build: ServiceCommandBuild<'_>) -> Result<String> {
    let paths = service_paths(build.config, build.name, build.service)?;
    Ok(format!(
        r#"service_name={name}
state_dir={state_dir}
pid_file="$state_dir/pid"
status_file="$state_dir/status"
ready_file="$state_dir/ready"
if [ ! -f "$pid_file" ]; then
  echo "[service] $service_name not running"
  rm -f "$ready_file"
  echo "stopped" > "$status_file"
  exit 0
fi
pid=$(cat "$pid_file" 2>/dev/null || true)
if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
  echo "[service] $service_name not running"
  rm -f "$pid_file" "$ready_file"
  echo "stopped" > "$status_file"
  exit 0
fi
echo "[service] stopping $service_name pid=$pid"
kill -INT -"$pid" 2>/dev/null || kill -INT "$pid" 2>/dev/null || true
sleep 1
if kill -0 "$pid" 2>/dev/null; then
  kill -TERM -"$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
fi
sleep 1
if kill -0 "$pid" 2>/dev/null; then
  kill -KILL -"$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
fi
rm -f "$pid_file" "$ready_file"
echo "stopped" > "$status_file"
echo "[service] $service_name stopped""#,
        name = shell_quote(build.name),
        state_dir = shell_quote(&paths.state_dir),
    ))
}

struct ServicePaths {
    state_dir: String,
    work_dir: String,
    log_file: String,
}

fn service_paths(config: &AppConfig, name: &str, service: &ServiceConfig) -> Result<ServicePaths> {
    let remote_root = RemotePath::parse(config.remote.path.as_str())?;
    let relative = if service.dir.trim().is_empty() {
        RelativePath::parse(".")?
    } else {
        RelativePath::parse(&service.dir)?
    };
    let work_dir = remote_root.join_relative(&relative);
    let state_dir = format!(
        "{}/.rdev/services/{name}",
        remote_root.as_str().trim_end_matches('/')
    );
    let log_file = format!("{state_dir}/output.log");
    Ok(ServicePaths {
        state_dir,
        work_dir: work_dir.as_str().to_owned(),
        log_file,
    })
}

fn service_ready_line(name: &str, service: &ServiceConfig) -> String {
    if service.url.trim().is_empty() {
        format!("[service] {name} ready")
    } else {
        format!("[service] {name} ready: {}", service.url)
    }
}

fn validate_service_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(err(error_info::CONFIG_INVALID).with_hint("service name cannot be empty"));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(err(error_info::CONFIG_INVALID).with_hint(format!(
            "service name may only contain letters, numbers, '-', '_' and '.': {name}"
        )));
    }
    Ok(())
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

    use crate::cli::{AliasDeleteArgs, AliasSetArgs, ServiceSetArgs};
    use crate::config::{AppConfig, CommandAliasConfig, ServiceConfig};
    use crate::path::{SyncExclusionExplanation, SyncExclusionReason};

    use super::{
        alias_delete, alias_set, build_service_logs_command, build_service_start_command,
        build_service_status_command, build_service_stop_command, build_service_wait_command,
        format_sync_exclusion, resolve_command_alias, service_list, service_set,
        AliasResolveRequest, ResolvedCommand, ServiceCommandBuild,
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

    #[test]
    fn service_list_formats_configured_services() {
        let root = temp_project("rdev-service-list-test");
        write_test_config(&root);
        let mut config = load_test_config(&root);
        config.services.insert(
            "backend".to_owned(),
            ServiceConfig {
                dir: "knota-fold".to_owned(),
                command: "cargo loco start --all".to_owned(),
                ready_pattern: "listening on".to_owned(),
                url: "http://10.124.124.0:5150".to_owned(),
            },
        );
        write_raw_config(&root, &config);

        let output = match service_list(&root) {
            Ok(output) => output,
            Err(error) => panic!("service list should succeed: {error}"),
        };

        assert!(output.contains("backend"));
        assert!(output.contains("dir=knota-fold"));
        assert!(output.contains("ready_pattern=listening on"));
        assert!(output.contains("http://10.124.124.0:5150"));
        let _cleanup = fs::remove_dir_all(root);
    }

    #[test]
    fn service_set_creates_and_updates_config_service() {
        let root = temp_project("rdev-service-set-test");
        write_test_config(&root);

        let created = service_set(
            ServiceSetArgs {
                name: "backend".to_owned(),
                dir: Some(PathBuf::from("knota-fold")),
                ready: "listening on".to_owned(),
                url: "http://10.124.124.0:5150".to_owned(),
                command: vec!["cargo loco start --all".to_owned()],
            },
            &root,
        );

        let created = match created {
            Ok(message) => message,
            Err(error) => panic!("service set should create: {error}"),
        };
        assert_eq!(created, "service created: backend");
        let config = load_test_config(&root);
        let service = match config.services.get("backend") {
            Some(service) => service,
            None => panic!("service should exist"),
        };
        assert_eq!(service.dir, "knota-fold");
        assert_eq!(service.ready_pattern, "listening on");
        assert_eq!(service.url, "http://10.124.124.0:5150");

        let updated = service_set(
            ServiceSetArgs {
                name: "backend".to_owned(),
                dir: Some(PathBuf::from("api")),
                ready: "ready".to_owned(),
                url: String::new(),
                command: vec!["pnpm dev --host".to_owned()],
            },
            &root,
        );

        let updated = match updated {
            Ok(message) => message,
            Err(error) => panic!("service set should update: {error}"),
        };
        assert_eq!(updated, "service updated: backend");
        let config = load_test_config(&root);
        let service = match config.services.get("backend") {
            Some(service) => service,
            None => panic!("service should exist"),
        };
        assert_eq!(service.dir, "api");
        assert_eq!(service.command, "pnpm dev --host");
        assert_eq!(service.ready_pattern, "ready");
        assert_eq!(service.url, "");
        let _cleanup = fs::remove_dir_all(root);
    }

    #[test]
    fn service_commands_use_remote_state_and_background_start() {
        let config = AppConfig::template("root@example.com", 22, "/root/project");
        let service = ServiceConfig {
            dir: "backend".to_owned(),
            command: "cargo loco start --all".to_owned(),
            ready_pattern: "listening on".to_owned(),
            url: "http://10.124.124.0:5150".to_owned(),
        };
        let build = ServiceCommandBuild {
            config: &config,
            name: "api",
            service: &service,
            value: 30,
        };

        let start = match build_service_start_command(build) {
            Ok(command) => command,
            Err(error) => panic!("build start: {error}"),
        };

        assert!(start.contains("/root/project/.rdev/services/api"));
        assert!(start.contains("setsid sh -lc"));
        assert!(start.contains("ready timeout after ${wait_timeout}s"));
        assert!(start.contains("still running pid=$pid"));
        assert!(start.contains("waiting elapsed=${elapsed}s timeout=${wait_timeout}s"));
        assert!(start.contains("[service] logs: rdev service logs $service_name"));
        assert!(start.contains("[service] remote_log=$log_file"));
        assert!(start.contains("rdev service logs $service_name"));
        assert!(start.contains("[service] api ready: http://10.124.124.0:5150"));

        let wait = match build_service_wait_command(ServiceCommandBuild {
            config: &config,
            name: "api",
            service: &service,
            value: 30,
        }) {
            Ok(command) => command,
            Err(error) => panic!("build wait: {error}"),
        };
        assert!(wait.contains("[service] waiting $service_name pid=$pid"));
        assert!(wait.contains("ready timeout after ${wait_timeout}s"));

        let status = match build_service_status_command(ServiceCommandBuild {
            config: &config,
            name: "api",
            service: &service,
            value: 0,
        }) {
            Ok(command) => command,
            Err(error) => panic!("build status: {error}"),
        };
        assert!(status.contains("status=$status"));
        assert!(status.contains("status=\"stale_ready\""));
        assert!(status.contains("remote_log=$log_file"));
        assert!(status.contains("logs_command=rdev service logs $service_name"));
        assert!(status.contains("wait_command=rdev service wait $service_name"));
        assert!(status.contains("printf 'url=%s\\n'"));

        let logs = match build_service_logs_command(ServiceCommandBuild {
            config: &config,
            name: "api",
            service: &service,
            value: 25,
        }) {
            Ok(command) => command,
            Err(error) => panic!("build logs: {error}"),
        };
        assert!(logs.contains("tail -n 25"));

        let stop = match build_service_stop_command(ServiceCommandBuild {
            config: &config,
            name: "api",
            service: &service,
            value: 0,
        }) {
            Ok(command) => command,
            Err(error) => panic!("build stop: {error}"),
        };
        assert!(stop.contains("kill -INT -\"$pid\""));
        assert!(stop.contains("[service] $service_name stopped"));
    }

    #[test]
    fn formats_sync_exclusion_reason() {
        let output = format_sync_exclusion(SyncExclusionExplanation {
            relative_path: Some("knota-fold/src/data/mod.rs".to_owned()),
            excluded: false,
            reason: SyncExclusionReason::MatchedRule {
                rule: "!src/data".to_owned(),
                include: true,
                pattern: "src/data".to_owned(),
            },
        });

        assert!(output.contains("path=knota-fold/src/data/mod.rs"));
        assert!(output.contains("status=included"));
        assert!(output.contains("matched include rule `!src/data`"));
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
        write_raw_config(root, &config);
    }

    fn write_raw_config(root: &std::path::Path, config: &AppConfig) {
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
