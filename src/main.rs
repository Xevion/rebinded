mod actions;
mod config;
mod platform;
mod state;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing::{info, Level};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "rebinded", about = "Cross-platform key remapping daemon")]
struct Args {
    /// Path to config file (default: ~/.config/rebinded/config.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rebinded")
        .join("config.toml")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    let filter = if args.verbose {
        EnvFilter::new(Level::DEBUG.to_string())
    } else {
        EnvFilter::from_default_env().add_directive(Level::INFO.into())
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config_path = args.config.unwrap_or_else(default_config_path);
    info!("loading config from {}", config_path.display());

    let config =
        config::Config::load(&config_path).context("failed to load configuration file")?;

    info!(
        "loaded {} bindings, {} debounce profiles",
        config.bindings.len(),
        config.debounce.len()
    );

    // Initialize platform-specific event loop
    platform::run(config).await
}
