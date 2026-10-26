use anyhow::Result;
use clap::Parser;

/// Cairnscope: explore any codebase as a graph + code, locally.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

fn main() -> Result<()> {
    let _cli = Cli::parse();
    Ok(())
}
