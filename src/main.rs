use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clor::ClorExt;

mod auth;
mod github;
mod list;
mod pulls;
mod tui;

/// A command line application.
#[derive(Debug, Parser)]
#[command(name = "zevl", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List the GitHub organizations you have access to.
    List,
    /// Log in to GitHub with a device code.
    Login,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nerror: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    // `clor_style` installs a custom help template, so it must be applied to the
    // fully built command before parsing instead of using `Cli::parse()`.
    let matches = Cli::command().clor_style().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());

    match cli.command {
        Some(Command::Login) => auth::login(),
        // `list` is the default action when no subcommand is given.
        Some(Command::List) | None => list::list(),
    }
}
