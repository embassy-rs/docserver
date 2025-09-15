#![feature(iterator_try_collect)]

use clap::{Parser, Subcommand};

mod commands;
mod common;

#[derive(Parser)]
#[command(name = "docserver")]
#[command(about = "A documentation server and builder tool")]
#[command(version = "1.0")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build documentation archives from crate source
    Build(commands::build::BuildArgs),
    /// Build documentation archives from crates.io release
    BuildRelease(commands::build_release::BuildReleaseArgs),
    /// Serve documentation from archives
    Serve(commands::serve::ServeArgs),
    /// Extract zup archives
    Unzup(commands::unzup::UnzupArgs),
    /// Compress a directory into a zup archive
    Zup(commands::zup::ZupArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => commands::build::run(args).await,
        Commands::BuildRelease(args) => commands::build_release::run(args).await,
        Commands::Serve(args) => commands::serve::run(args).await,
        Commands::Unzup(args) => commands::unzup::run(args).await,
        Commands::Zup(args) => commands::zup::run(args).await,
    }
}
