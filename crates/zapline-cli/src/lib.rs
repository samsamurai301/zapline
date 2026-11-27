//! # zapline-cli
//!
//! The `zapline` command-line tool: project scaffolding, code generators, a
//! live-reload dev server, route and OpenAPI inspection, benchmarking and
//! toolchain diagnostics.
//!
//! The binary is a thin wrapper around [`main_with`]; everything is also
//! available as a library so it can be tested.

#![warn(missing_docs)]

pub mod bench;
pub mod brief;
pub mod build;
pub mod cli;
pub mod client;
pub mod db;
pub mod dev;
pub mod doctor;
pub mod generate;
pub mod mcp;
pub mod naming;
pub mod new;
pub mod openapi;
pub mod project;
pub mod render;
pub mod routes;
pub mod tailwind;
pub mod templates;
pub mod ui;

#[cfg(test)]
mod test_util;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches};

use cli::{Cli, Command, DbCommand, GenerateKind};
use project::Project;

/// How the binary was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invocation {
    /// `zapline <command>`.
    Standalone,
    /// `cargo zapline <command>`: cargo runs `cargo-zapline zapline <command>`.
    CargoSubcommand,
}

/// Entry point shared by the `zapline` and `cargo-zapline` binaries.
pub fn main_with(invocation: Invocation) -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    match run_from(args, invocation) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Clap(err)) => {
            // Prints help/version to stdout and usage errors to stderr.
            let _ = err.print();
            ExitCode::from(u8::try_from(err.exit_code()).unwrap_or(2))
        }
        Err(Failure::Other(err)) => {
            eprintln!("{}", format_error_chain(&err, ui::stderr_color()));
            ExitCode::FAILURE
        }
    }
}

/// Why a run did not succeed.
#[derive(Debug)]
pub enum Failure {
    /// Bad command line (or `--help`/`--version`, which clap reports this way).
    Clap(clap::Error),
    /// A command failed.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Other(e)
    }
}

/// `error: message` followed by `caused by:` lines.
pub fn format_error_chain(err: &anyhow::Error, color: bool) -> String {
    let mut out = ui::format_error(color, err);
    for cause in err.chain().skip(1) {
        out.push_str(&format!("\n{} {cause}", ui::dim(color, "  caused by:")));
    }
    out
}

/// Parses `args` (including the program name) and runs the command.
pub fn run_from(mut args: Vec<OsString>, invocation: Invocation) -> Result<(), Failure> {
    let mut command = Cli::command();
    if invocation == Invocation::CargoSubcommand {
        // `cargo zapline new x` runs `cargo-zapline zapline new x`.
        if args.get(1).is_some_and(|a| a == "zapline") {
            args.remove(1);
        }
        command = command.bin_name("cargo zapline");
    }
    let matches = command.try_get_matches_from(args).map_err(Failure::Clap)?;
    let cli = Cli::from_arg_matches(&matches).map_err(Failure::Clap)?;
    execute(cli).map_err(Failure::Other)
}

/// Runs a parsed command line.
pub fn execute(cli: Cli) -> Result<()> {
    if let Some(dir) = &cli.dir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("cannot change to directory {}", dir.display()))?;
    }
    match cli.command {
        Command::New(a) => new::run(&new::NewOptions {
            name: a.name,
            template: a.template,
            parent: a.path,
            zapline_path: a.zapline_path,
            git: !a.no_git,
        }),
        Command::Generate(g) => {
            let here = Path::new(".");
            let report = match g.kind {
                GenerateKind::Resource {
                    name,
                    fields,
                    plural,
                    memory,
                } => generate::resource_with_fields(
                    here,
                    &name,
                    plural.as_deref(),
                    &fields,
                    memory,
                    g.force,
                )?,
                GenerateKind::Handler { name, method, path } => {
                    generate::handler(here, &name, method, path.as_deref(), g.force)?
                }
                GenerateKind::Middleware { name } => generate::middleware(here, &name, g.force)?,
                GenerateKind::Auth => generate::auth::generate(&Project::discover(here)?, g.force)?,
                GenerateKind::Job { name } => {
                    generate::job::generate(&Project::discover(here)?, &name, g.force)?
                }
                GenerateKind::Deploy { target } => {
                    generate::deploy::generate(&Project::discover(here)?, target, g.force)?
                }
                GenerateKind::Tailwind => tailwind::generate(&Project::discover(here)?, g.force)?,
            };
            report.print();
            Ok(())
        }
        Command::Dev(a) => {
            let project = Project::discover(Path::new("."))?;
            dev::run(
                &project,
                &dev::DevOptions {
                    bin: a.bin,
                    release: a.release,
                    app_args: a.app_args,
                    debounce: Duration::from_millis(a.debounce_ms),
                    client: match a.client {
                        Some(c) => Some(openapi::resolve_output(
                            &std::env::current_dir()
                                .context("cannot determine the current directory")?,
                            &c,
                        )),
                        None => None,
                    },
                    ..dev::DevOptions::default()
                },
            )
        }
        Command::Routes(a) => {
            let project = Project::discover(Path::new("."))?;
            routes::run(&project, a.bin.as_deref(), a.json)
        }
        Command::Openapi(a) => {
            let project = Project::discover(Path::new("."))?;
            openapi::run(&project, a.bin.as_deref(), &a.output)
        }
        Command::Db(DbCommand::New { name }) => {
            let project = Project::discover(Path::new("."))?;
            db::new(&project.root, &name)
        }
        Command::Db(DbCommand::Migrate { bin }) => {
            let project = Project::discover(Path::new("."))?;
            db::migrate(&project, bin.as_deref())
        }
        Command::Db(DbCommand::Seed { bin }) => {
            let project = Project::discover(Path::new("."))?;
            db::seed(&project, bin.as_deref())
        }
        Command::Check { clippy } => {
            let project = Project::discover(Path::new("."))?;
            finish(brief::check(&project, clippy)?)
        }
        Command::Test { filter } => {
            let project = Project::discover(Path::new("."))?;
            finish(brief::test(&project, filter.as_deref())?)
        }
        Command::Client(a) => {
            let project = Project::discover(Path::new("."))?;
            client::run(&project, a.bin.as_deref(), &a.output)
        }
        Command::Css { minify, watch } => {
            let project = Project::discover(Path::new("."))?;
            if watch {
                let (mut child, _stdin) = tailwind::watch(&project.root)?;
                child.wait().context("tailwindcss failed")?;
                Ok(())
            } else {
                tailwind::build(&project.root, minify)
            }
        }
        Command::Mcp => mcp::run(),
        Command::Doctor => doctor::run(),
        Command::Bench(a) => {
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
            let cfg = bench::configure(&a.url, a.connections, &a.duration, a.threads, cpus)?;
            bench::run(&cfg, a.raw)
        }
    }
}

/// Prints a condensed report; a failed check or test run is an error exit
/// without another message (the report already says what failed).
fn finish((ok, report): (bool, String)) -> Result<()> {
    print!("{report}");
    if ok {
        Ok(())
    } else {
        std::process::exit(1)
    }
}
