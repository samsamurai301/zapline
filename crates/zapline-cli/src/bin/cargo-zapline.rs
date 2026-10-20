//! The `cargo-zapline` binary: `cargo zapline <command>` is `zapline <command>`.
//!
//! Cargo invokes external subcommands as `cargo-zapline zapline <args>`; the
//! leading `zapline` is stripped by [`zapline_cli::run_from`].

fn main() -> std::process::ExitCode {
    zapline_cli::main_with(zapline_cli::Invocation::CargoSubcommand)
}
