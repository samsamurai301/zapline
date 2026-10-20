//! The `zapline` binary.

fn main() -> std::process::ExitCode {
    zapline_cli::main_with(zapline_cli::Invocation::Standalone)
}
