//! The `vidl` binary. Every flag it accepts and everything it does with them live in the
//! library ([`vidl::Args`] and [`vidl::run`]), so an embedder gets the same command line and the
//! same behaviour without reimplementing either — and this file cannot drift from it.

use std::process::ExitCode;

use clap::Parser;

#[derive(Parser)]
#[command(name = "vidl", version, about = "Download a video with yt-dlp: YouTube playlists and \
                                           channels build folder trees; any other site \
                                           downloads flat")]
struct Cli {
    #[command(flatten)]
    args: vidl::Args,
}

fn main() -> ExitCode {
    let code = vidl::run(Cli::parse().args);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
