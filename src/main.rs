mod business;
mod cli;
mod config;
mod daemon;
mod menu;
mod niri;
mod pinning;
mod protocol;
mod state_store;

use anyhow::Result;
use std::env;
use std::ffi::{OsStr, OsString};

use daemon::Takeover;

/// What the first argument asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Invocation {
    /// Start the daemon, taking the socket over if asked to.
    Daemon(Takeover),
    /// A subcommand: the CLI talks to the daemon instead.
    Cli,
}

/// Only the first argument decides: everything after it belongs to the CLI,
/// which parses its own options with clap.
fn parse_invocation(first: Option<OsString>) -> Invocation {
    match first.as_deref() {
        None => Invocation::Daemon(Takeover::Refuse),
        Some(argument) if argument == OsStr::new("--replace") => {
            Invocation::Daemon(Takeover::Replace)
        }
        Some(_) => Invocation::Cli,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_target(false)
        .with_line_number(false)
        .without_time()
        .init();

    // The daemon is started bare (or with --replace to take over a socket);
    // any other argument means CLI mode.
    match parse_invocation(env::args_os().nth(1)) {
        Invocation::Daemon(takeover) => daemon::start(takeover).await,
        Invocation::Cli => cli::run_cli().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_argument_starts_the_daemon() {
        assert_eq!(parse_invocation(None), Invocation::Daemon(Takeover::Refuse));
    }

    #[test]
    fn test_replace_starts_the_daemon_in_takeover_mode() {
        assert_eq!(
            parse_invocation(Some(OsString::from("--replace"))),
            Invocation::Daemon(Takeover::Replace)
        );
    }

    #[test]
    fn test_any_other_argument_goes_to_the_cli() {
        for argument in [
            "sticky",
            "list",
            "--version",
            "--",
            "",
            "-h",
            "--replace-now",
        ] {
            assert_eq!(
                parse_invocation(Some(OsString::from(argument))),
                Invocation::Cli,
                "{argument:?}"
            );
        }
    }
}
