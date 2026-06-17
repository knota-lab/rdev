use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "rdev",
    version,
    about = "Local editing, remote building developer tool"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Init(InitArgs),
    AuthCheck,
    Doctor,
    Run(RunArgs),
    Sync(SyncArgs),
    Up(UpArgs),
    Status,
    Stop,
    Ssh(SshArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub path: Option<String>,
    #[arg(long, default_value_t = 22)]
    pub port: u16,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    pub command: String,
    #[arg(long)]
    pub dir: Option<PathBuf>,
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub no_delete: bool,
}

#[derive(Debug, Args)]
pub struct UpArgs {
    #[arg(long)]
    pub no_initial_sync: bool,
    #[arg(long)]
    pub poll: bool,
    #[arg(long)]
    pub tui: bool,
}

#[derive(Debug, Args)]
pub struct SshArgs {
    #[arg(long)]
    pub dir: Option<PathBuf>,
}
