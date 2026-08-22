//! cordis-cli entry point: parses command-line arguments and starts the
//! runtime.

use clap::{Parser, Subcommand};

/// Cordis plugin application launcher.
#[derive(Parser)]
#[command(name = "cordis", version, about)]
struct Cli {
    /// Config file (yaml/json); default `cordis.yml`.
    #[arg(short, long)]
    config: Option<String>,
    /// Directory to scan for `.so` plugins; default `plugins`.
    #[arg(long)]
    plugins_dir: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffolds a new cordis project.
    Create {
        /// Directory to scaffold into.
        name: String,
        /// Overwrite an existing directory.
        #[arg(short, long)]
        force: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Create { name, force }) = cli.command {
        return cordis_cli::create_project(std::path::Path::new(&name), force);
    }

    let options = cordis_cli::CliOptions {
        config: cli.config,
        plugins_dir: cli.plugins_dir,
    };
    // Stage 2 of the multithreading plan: tasks are `Send` and run on the
    // worker pool directly; no `LocalSet` pinning is needed anymore.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async { cordis_cli::run(&options).await })?;
    Ok(())
}
