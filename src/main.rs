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
    /// Serve documentation from archives
    Serve(commands::serve::ServeArgs),
    /// Extract and inspect zup archives for debugging
    Extract(commands::extract::ExtractArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => commands::build::run(args).await,
        Commands::Serve(args) => commands::serve::run(args).await,
        Commands::Extract(args) => commands::extract::run(args).await,
    }
}
