use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rdev",
    version,
    about = "Local editing, remote build/run workflow tool",
    long_about = "rdev keeps local editing and remote build/run workflows in one tool.\n\nUse `rdev up --tui` for the integrated sync/session console.\nUse `rdev exec` for Codex/Claude-style repeated remote commands through a persistent project daemon.\nUse `rdev run` for one-shot SSH command execution without the daemon."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Create .rdev/config.toml for the current project")]
    Init(InitArgs),
    #[command(
        about = "Manage remote command aliases",
        long_about = "Manage project-level remote command aliases stored in .rdev/config.toml.\n\nAliases are only expanded by `rdev run` and `rdev exec`. They target the configured remote server and are not used by the TUI session console or local shell commands."
    )]
    Alias(AliasArgs),
    #[command(about = "Check internal SSH authentication")]
    AuthCheck,
    #[command(about = "Explain whether a path is ignored by sync exclude rules")]
    WhyIgnore(WhyIgnoreArgs),
    #[command(
        about = "Manage the project daemon used by rdev exec",
        long_about = "Manage the local project daemon used by `rdev exec`.\n\nThe daemon listens on 127.0.0.1, stores state in .rdev/daemon.json, and keeps one persistent SSH connection to the configured remote."
    )]
    Daemon(DaemonArgs),
    #[command(about = "Check local and remote requirements")]
    Doctor,
    #[command(
        about = "Execute a remote command through the persistent project daemon",
        long_about = "Execute a remote command through the persistent project daemon.\n\nThe daemon keeps one SSH connection open, so repeated exec calls avoid reconnecting. stdout/stderr are streamed to the local terminal. Press Ctrl+C to cancel the remote process; rdev exits with code 130.\n\nThe command argument may be either a literal remote shell command or a configured remote command alias from [commands.*]. Alias `dir` is resolved relative to the configured remote project path.\n\nUse `--summary` to write full output to .rdev/logs and print only a compact result summary.\n\nExamples:\n  rdev exec \"pwd\"\n  rdev exec \"cargo test\"\n  rdev exec --dir backend \"cargo test\"\n  rdev exec --summary \"cargo test\"\n  rdev exec backend-lint\n  rdev exec --summary l2-session -- session_id=26"
    )]
    Exec(ExecArgs),
    #[command(
        about = "Run a one-shot remote command over SSH",
        long_about = "Run a one-shot remote command over SSH.\n\nThis command does not use the persistent daemon. Prefer `rdev exec` for Codex/Claude-style repeated remote command execution.\n\nThe command argument may be either a literal remote shell command or a configured remote command alias from [commands.*]. Alias `dir` is resolved relative to the configured remote project path."
    )]
    Run(RunArgs),
    #[command(
        about = "Start configured remote services and detect readiness",
        long_about = "Start configured remote services from [services.*] in .rdev/config.toml.\n\n`rdev service start <name>` starts the service in the remote background, prints low-frequency heartbeat while waiting for ready_pattern, then exits successfully so coding agents can continue. Use `--logs` when you need live service logs instead of heartbeat. Use `rdev service status/logs/stop <name>` to inspect or stop the background service.\n\nExample config:\n  [services.backend]\n  dir = \"knota-fold\"\n  command = \"cargo loco start --all\"\n  ready_pattern = \"listening on\"\n  url = \"http://10.124.124.0:5150\""
    )]
    Service(ServiceArgs),
    #[command(about = "Run one full sync and exit")]
    Sync(SyncArgs),
    #[command(about = "Start file watching, sync, and optional TUI process console")]
    Up(UpArgs),
    #[command(about = "Show current up process status")]
    Status,
    #[command(about = "Request the current up process to stop")]
    Stop,
    #[command(about = "Open a remote SSH shell in the configured project directory")]
    Ssh(SshArgs),
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Debug, Args)]
pub struct AliasArgs {
    #[command(subcommand)]
    pub command: AliasCommand,
}

#[derive(Debug, Subcommand)]
pub enum AliasCommand {
    #[command(about = "List configured remote command aliases")]
    List,
    #[command(
        about = "Create or update a remote command alias",
        long_about = "Create or update a remote command alias in .rdev/config.toml.\n\nThe alias is expanded only by `rdev run <alias>` and `rdev exec <alias>`. It is not a TUI session alias and it is not a local shell alias.\n\nExamples:\n  rdev alias set backend-lint --dir knota-fold -- cargo clippy --all-features -- -D warnings\n  rdev alias set l2-session --dir knota-fold -- cargo run -- task l2_process_session session_id:{session_id}"
    )]
    Set(AliasSetArgs),
    #[command(about = "Delete a remote command alias")]
    Delete(AliasDeleteArgs),
}

#[derive(Debug, Args)]
pub struct AliasSetArgs {
    #[arg(help = "Alias name")]
    pub name: String,
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
    #[arg(last = true, required = true, help = "Remote command to store")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AliasDeleteArgs {
    #[arg(help = "Alias name")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct WhyIgnoreArgs {
    #[arg(help = "Project-relative or absolute local path to inspect")]
    pub path: PathBuf,
}

#[derive(Debug, Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    #[command(about = "List configured services")]
    List,
    #[command(
        about = "Create or update a configured service",
        long_about = "Create or update a configured service in .rdev/config.toml.\n\nExamples:\n  rdev service set backend --dir knota-fold --ready \"listening on\" --url http://10.124.124.0:5150 -- cargo loco start --all\n  rdev service start backend"
    )]
    Set(ServiceSetArgs),
    #[command(about = "Start a configured service in the background and wait until ready")]
    Start(ServiceStartArgs),
    #[command(about = "Wait for an already started service to become ready")]
    Wait(ServiceWaitArgs),
    #[command(about = "Show configured service runtime status")]
    Status(ServiceStatusArgs),
    #[command(about = "Print configured service logs")]
    Logs(ServiceLogsArgs),
    #[command(about = "Stop a configured background service")]
    Stop(ServiceStopArgs),
}

#[derive(Debug, Args)]
pub struct ServiceSetArgs {
    #[arg(help = "Service name")]
    pub name: String,
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
    #[arg(long, help = "Ready log pattern to detect")]
    pub ready: String,
    #[arg(
        long,
        default_value = "",
        help = "URL to print when the service is ready"
    )]
    pub url: String,
    #[arg(last = true, required = true, help = "Remote service command to store")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServiceStartArgs {
    #[arg(help = "Configured service name from [services.<name>]")]
    pub name: String,
    #[arg(
        long,
        help = "Stream service logs while waiting; disables heartbeat output"
    )]
    pub logs: bool,
    #[arg(
        long,
        default_value_t = 600,
        help = "Seconds to wait for ready_pattern"
    )]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct ServiceWaitArgs {
    #[arg(help = "Configured service name from [services.<name>]")]
    pub name: String,
    #[arg(
        long,
        help = "Stream service logs while waiting; disables heartbeat output"
    )]
    pub logs: bool,
    #[arg(
        long,
        default_value_t = 600,
        help = "Seconds to wait for ready_pattern"
    )]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct ServiceStatusArgs {
    #[arg(help = "Configured service name from [services.<name>]")]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ServiceLogsArgs {
    #[arg(help = "Configured service name from [services.<name>]")]
    pub name: String,
    #[arg(long, default_value_t = 80, help = "Number of log lines to print")]
    pub lines: u32,
}

#[derive(Debug, Args)]
pub struct ServiceStopArgs {
    #[arg(help = "Configured service name from [services.<name>]")]
    pub name: String,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    #[command(about = "Start the local project daemon if needed")]
    Start,
    #[command(about = "Show daemon pid, remote, busy state, and active job")]
    Status,
    #[command(about = "Stop the local project daemon")]
    Stop,
    #[command(hide = true)]
    Serve,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[arg(help = "Remote shell command or configured remote command alias to execute")]
    pub command: String,
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
    #[arg(
        long,
        help = "Write full output to .rdev/logs and print a compact summary"
    )]
    pub summary: bool,
    #[arg(
        last = true,
        help = "Remote command alias parameters as key=value pairs"
    )]
    pub params: Vec<String>,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long, help = "Remote SSH target, for example root@example.com")]
    pub host: Option<String>,
    #[arg(long, help = "Remote project directory, for example /root/project")]
    pub path: Option<String>,
    #[arg(long, default_value_t = 22, help = "Remote SSH port")]
    pub port: u16,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(help = "Remote shell command or configured remote command alias to execute")]
    pub command: String,
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
    #[arg(long, help = "Skip sync before running the command")]
    pub no_sync: bool,
    #[arg(
        last = true,
        help = "Remote command alias parameters as key=value pairs"
    )]
    pub params: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long, help = "Preview sync actions without changing remote files")]
    pub dry_run: bool,
    #[arg(long, help = "Disable remote delete propagation for this sync")]
    pub no_delete: bool,
}

#[derive(Debug, Args)]
pub struct UpArgs {
    #[arg(long, help = "Skip initial full sync before watching")]
    pub no_initial_sync: bool,
    #[arg(long, help = "Use polling file watcher")]
    pub poll: bool,
    #[arg(long, help = "Start the ratatui process console")]
    pub tui: bool,
}

#[derive(Debug, Args)]
pub struct SshArgs {
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
}
