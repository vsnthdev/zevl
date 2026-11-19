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
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    // the `list` options are flattened into the root command so that a bare
    // `zevl` invocation behaves exactly like `zevl list`, flags included.
    #[command(flatten)]
    list: list::ListArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List open pull requests in your personal account or an organization.
    List(list::ListArgs),
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
        Some(Command::List(args)) => list::list(args),
        // a bare `zevl` invocation is an alias for `zevl list`.
        None => list::list(cli.list),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_accepts_the_list_options() {
        let cli = Cli::try_parse_from(["zevl", "--scope", "introbotai"])
            .expect("the root command should accept list options");

        assert!(cli.command.is_none());
        assert_eq!(cli.list.scope.as_deref(), Some("introbotai"));
    }

    #[test]
    fn list_subcommand_accepts_the_shared_list_options() {
        let cli = Cli::try_parse_from(["zevl", "list", "--scope", "introbotai"])
            .expect("the list subcommand should accept the shared options");

        match cli.command {
            Some(Command::List(args)) => {
                assert_eq!(args.scope.as_deref(), Some("introbotai"));
            }
            _ => panic!("expected the list subcommand"),
        }
    }
}
