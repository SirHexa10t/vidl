//! The `vidl` binary. Every flag it accepts and everything it does with them live in the
//! library ([`vidl::Cli`]), so an embedder gets the same command line and the same behaviour
//! without reimplementing either — and this file cannot drift from it.

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let code = vidl::Cli::parse().run();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
