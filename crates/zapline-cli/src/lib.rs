//! # zapline-cli
//!
//! The `zapline` command-line tool: project scaffolding, one-file apps, code
//! generators, a live-reload dev server, route and OpenAPI inspection,
//! benchmarking and toolchain diagnostics.
//!
//! The binary is a thin wrapper around [`main_with`]; everything is also
//! available as a library so it can be tested.

#![warn(missing_docs)]

pub mod bench;
pub mod brief;
pub mod build;
pub mod cli;
pub mod client;
pub mod codes;
pub mod db;
pub mod dev;
pub mod doctor;
pub mod fuzz;
pub mod generate;
pub mod live;
pub mod mcp;
pub mod naming;
pub mod new;
pub mod openapi;
pub mod project;
pub mod render;
pub mod routes;
pub mod script;
pub mod ship;
pub mod snap;
pub mod tailwind;
pub mod templates;
pub mod ui;

#[cfg(test)]
mod test_util;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
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
        Command::New(a) if a.name.ends_with(".rs") => {
            if let Some(t) = a.template {
                anyhow::bail!(
                    "`--template {}` makes a project; `{}` is a one-file app\n\
                     hint: drop `.rs` from the name for a project",
                    t.name(),
                    a.name
                );
            }
            let file = if a.path == Path::new(".") {
                PathBuf::from(&a.name)
            } else {
                a.path.join(&a.name)
            };
            script::run_new(&file, a.zapline_path.as_deref())
        }
        Command::New(a) => new::run(&new::NewOptions {
            name: a.name,
            template: a.template.unwrap_or(templates::Template::Api),
            parent: a.path,
            zapline_path: a.zapline_path,
            git: !a.no_git,
        }),
        Command::Run(a) => script::run(&script::RunOptions {
            file: a.file(),
            release: a.release,
            args: a.app_args(),
            print_manifest: a.print_manifest,
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
                GenerateKind::Migration { name, fields } => {
                    generate::migration(here, &name, &fields)?
                }
                GenerateKind::Handler { name, method, path } => {
                    generate::handler(here, &name, method, path.as_deref(), g.force)?
                }
                GenerateKind::Middleware { name } => generate::middleware(here, &name, g.force)?,
                GenerateKind::Auth => generate::auth::generate(&Project::discover(here)?, g.force)?,
                GenerateKind::Job { name } => {
                    generate::job::generate(&Project::discover(here)?, &name, g.force)?
                }
                GenerateKind::Mailer { name } => {
                    generate::mailer::generate(&Project::discover(here)?, &name, g.force)?
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
            // A one-file app has no project to configure clients in: only
            // `--client` applies there.
            let project = match &a.file {
                Some(_) => None,
                None => Some(Project::discover(Path::new("."))?),
            };
            let clients = match (a.no_client, a.client, &project) {
                (true, _, _) => client::Configured::default(),
                (false, Some(c), _) => client::Configured {
                    paths: vec![openapi::resolve_output(
                        &std::env::current_dir()
                            .context("cannot determine the current directory")?,
                        &c,
                    )],
                    explicit: true,
                },
                (false, None, Some(project)) => client::configured(project)?,
                (false, None, None) => client::Configured::default(),
            };
            let opts = dev::DevOptions {
                bin: a.bin,
                release: a.release,
                app_args: a.app_args,
                debounce: Duration::from_millis(a.debounce_ms),
                proxy: !a.no_proxy,
                clients: clients.paths,
                clients_explicit: clients.explicit,
                record: a.record,
                ..dev::DevOptions::default()
            };
            match (&a.file, &project) {
                (Some(file), _) => dev::run_script(&script::open(file)?, &opts),
                (None, Some(project)) => dev::run(project, &opts),
                (None, None) => unreachable!("a project was discovered above"),
            }
        }
        Command::Snap(a) => {
            let url = a
                .url
                .unwrap_or_else(|| snap::default_url(|k| std::env::var(k).ok()));
            snap::run(&a.name, &url)
        }
        Command::Routes(a) => {
            let project = project_or_script(a.file.as_deref())?;
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
        Command::Check { file, clippy } => {
            let project = project_or_script(file.as_deref())?;
            finish(brief::check(&project, clippy)?)
        }
        Command::Test { first, filter } => {
            // `zapline test [app.rs] [filter]`: a first argument ending in
            // `.rs` is a one-file app, anything else is the filter.
            let (file, filter) = match (first, filter) {
                (Some(f), filter) if f.ends_with(".rs") => (Some(PathBuf::from(f)), filter),
                (first, None) => (None, first),
                (Some(f), Some(_)) => anyhow::bail!(
                    "`{f}` is not a one-file app (a path ending in .rs); give one test filter"
                ),
                (None, Some(filter)) => (None, Some(filter)),
            };
            let project = project_or_script(file.as_deref())?;
            finish(brief::test(&project, filter.as_deref())?)
        }
        Command::Fuzz { seed, cases } => {
            let project = Project::discover(Path::new("."))?;
            if fuzz::run(&project, seed, cases)? {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Command::Client(a) => {
            let project = Project::discover(Path::new("."))?;
            client::run(&project, a.bin.as_deref(), a.output.as_deref())
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
        Command::Explain { code } => {
            match code {
                Some(code) => print!("{}", codes::explain(&code)?),
                None => print!("{}", codes::list()),
            }
            Ok(())
        }
        Command::Doctor => doctor::run(),
        Command::Ship(a) => {
            let project = Project::discover(Path::new("."))?;
            ship::run(
                &project,
                &ship::ShipOptions {
                    host: a.host,
                    bin: a.bin,
                    name: a.name,
                    port: a.port,
                    health: a.health,
                    target: a.target,
                    static_build: a.static_build,
                    ssh_port: a.ssh_port,
                    identity: a.identity,
                    env: a.env,
                    keep: usize::from(a.keep),
                    dry_run: a.dry_run,
                    rollback: a.rollback,
                },
            )
        }
        Command::Bench(a) => {
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
            let cfg = bench::configure(&a.url, a.connections, &a.duration, a.threads, cpus)?;
            bench::run(&cfg, a.raw)
        }
    }
}

/// The hidden project of a one-file app, or the project around the current directory.
fn project_or_script(file: Option<&Path>) -> Result<Project> {
    match file {
        Some(file) => script::project(file),
        None => Project::discover(Path::new(".")),
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
