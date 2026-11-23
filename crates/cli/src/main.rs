//! migrate-db command-line interface.

mod cli;
mod commands;
mod context;
mod output;
mod providers;
mod signals;
mod templates;
mod update;

use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use migrate_db_core::error::{Error, exit};

use crate::cli::{BaselineCommand, Cli, Command};
use crate::commands::migrate::{Invocation, parse_version};
use crate::output::Output;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => exit::OK,
                _ => exit::USAGE,
            };
            let _ = e.print();
            return to_exit_code(code);
        }
    };
    let out = Output::new(&cli.global);
    let code = match dispatch(&cli, &out).await {
        Ok(code) => code,
        Err(e) => {
            out.error(cli.command.name(), &e);
            e.exit_code()
        }
    };
    // Last, and on stderr only: stdout, JSON and the exit code stay untouched.
    if let Some(notice) = update::notice(cli.global.no_update_check).await {
        eprintln!("{notice}");
    }
    to_exit_code(code)
}

fn to_exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

async fn dispatch(cli: &Cli, out: &Output) -> Result<i32, Error> {
    let g = &cli.global;
    match &cli.command {
        Command::Init(args) => commands::init::run(g, out, args).await,
        Command::New {
            description,
            irreversible,
            repeatable,
            folder,
        } => commands::new::run(
            g,
            out,
            &commands::new::NewArgs {
                description,
                irreversible: *irreversible,
                repeatable: *repeatable,
                folder: folder.as_deref(),
            },
        ),
        Command::Status { connect } => commands::migrate::status(g, out, connect).await,
        Command::Validate { offline, connect } => {
            commands::validate::run(g, out, *offline, connect).await
        }
        Command::Up {
            to,
            steps,
            out_of_order,
            run,
        } => {
            let to = to.as_deref().map(parse_version).transpose()?;
            let request = migrate_db_core::planner::Request::Up { to, steps: *steps };
            commands::migrate::execute(
                g,
                out,
                Invocation {
                    command: "up",
                    request,
                    run,
                    allow_out_of_order: out_of_order.allowed,
                    allow_down: false,
                    yes: false,
                },
            )
            .await
        }
        Command::Down {
            to,
            steps,
            yes,
            allow_down,
            run,
        } => {
            let to = to.as_deref().map(parse_version).transpose()?;
            let request = migrate_db_core::planner::Request::Down { to, steps: *steps };
            commands::migrate::execute(
                g,
                out,
                Invocation {
                    command: "down",
                    request,
                    run,
                    allow_out_of_order: false,
                    allow_down: allow_down.allowed,
                    yes: *yes,
                },
            )
            .await
        }
        Command::Goto {
            target_version,
            out_of_order,
            yes,
            allow_down,
            run,
        } => {
            let target = parse_version(target_version)?;
            let request = migrate_db_core::planner::Request::Goto { target };
            commands::migrate::execute(
                g,
                out,
                Invocation {
                    command: "goto",
                    request,
                    run,
                    allow_out_of_order: out_of_order.allowed,
                    allow_down: allow_down.allowed,
                    yes: *yes,
                },
            )
            .await
        }
        Command::Baseline(BaselineCommand::Mark {
            version,
            description,
            mark_repeatables,
            force_unverified,
            yes,
            run,
        }) => {
            commands::baseline::mark(
                g,
                out,
                commands::baseline::MarkArgs {
                    version,
                    description: description.as_deref(),
                    mark_repeatables: *mark_repeatables,
                    force_unverified: *force_unverified,
                    yes: *yes,
                    run,
                },
            )
            .await
        }
        Command::Unlock {
            force: _,
            kill_holder,
            yes,
            connect,
        } => commands::unlock::run(g, out, *kill_holder, *yes, connect).await,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// clap's own consistency checks (unique ids, valid defaults, …).
    #[test]
    fn cli_definition_is_consistent() {
        crate::cli::Cli::command().debug_assert();
    }
}
