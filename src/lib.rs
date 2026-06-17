#![deny(clippy::too_many_arguments)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

pub mod app;
pub mod auth;
pub mod cli;
pub mod command;
pub mod config;
pub mod doctor;
pub mod error;
pub mod error_info;
pub mod path;
pub mod process;
pub mod session;
pub mod sftp;
pub mod ssh;
pub mod ssh_tar;
pub mod sync;
pub mod tui;
pub mod up;
