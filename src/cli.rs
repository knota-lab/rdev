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
    #[command(about = "Check internal SSH authentication")]
    AuthCheck,
    #[command(
        about = "Manage the project daemon used by rdev exec",
        long_about = "Manage the local project daemon used by `rdev exec`.\n\nThe daemon listens on 127.0.0.1, stores state in .rdev/daemon.json, and keeps one persistent SSH connection to the configured remote."
    )]
    Daemon(DaemonArgs),
    #[command(about = "Check local and remote requirements")]
    Doctor,
    #[command(
        about = "Execute a remote command through the persistent project daemon",
        long_about = "Execute a remote command through the persistent project daemon.\n\nThe daemon keeps one SSH connection open, so repeated exec calls avoid reconnecting. stdout/stderr are streamed to the local terminal. Press Ctrl+C to cancel the remote process; rdev exits with code 130.\n\nExamples:\n  rdev exec \"pwd\"\n  rdev exec \"cargo test\"\n  rdev exec --dir backend \"cargo test\""
    )]
    Exec(ExecArgs),
    #[command(
        about = "Run a one-shot remote command over SSH",
        long_about = "Run a one-shot remote command over SSH.\n\nThis command does not use the persistent daemon. Prefer `rdev exec` for Codex/Claude-style repeated remote command execution."
    )]
    Run(RunArgs),
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
    #[arg(help = "Remote shell command to execute")]
    pub command: String,
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
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
    #[arg(help = "Remote shell command to execute")]
    pub command: String,
    #[arg(long, help = "Project-relative remote working directory")]
    pub dir: Option<PathBuf>,
    #[arg(long, help = "Skip sync before running the command")]
    pub no_sync: bool,
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
