#![deny(clippy::too_many_arguments)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

use clap::Parser;
use rdev::cli::Cli;
use rdev::error::format_error;

fn main() {
    let cli = Cli::parse();
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(source) => {
            eprintln!("failed to read current directory: {source}");
            std::process::exit(70);
        }
    };

    match rdev::app::run(cli, &cwd) {
        Ok(message) => println!("{message}"),
        Err(error) => {
            let exit_code = error.exit_code();
            eprintln!("{}", format_error(&error));
            std::process::exit(exit_code);
        }
    }
}
