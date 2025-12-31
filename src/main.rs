mod config;
mod key;
mod platform;
mod strategy;

use clap::Parser;
use config::{Action, ActionSpec, RuntimeConfig};
use key::KeyEvent;
use platform::{EventResponse, Platform};
use std::path::PathBuf;
use std::process::ExitCode;
use strategy::{PlatformHandle, StrategyContext};
use tracing::{Level, debug, info};
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();

    // Initialize logging
    let filter = if args.verbose {
        EnvFilter::new(Level::DEBUG.to_string())
    } else {
        EnvFilter::from_default_env().add_directive(Level::INFO.into())
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Load and validate config
    let config_path = args.config.unwrap_or_else(default_config_path);
    info!("loading config from {}", config_path.display());

    let (config, runtime_config) = match config::load(&config_path) {
        Ok(result) => result,
        Err(err) => {
            // Use miette's fancy error display
            eprintln!("{:?}", miette::Report::new(err));
            return ExitCode::FAILURE;
        }
    };

    info!(
        "loaded {} bindings, {} strategies",
        config.bindings.len(),
        config.strategies.len()
    );

    info!(
        "resolved {} key bindings, {} strategies",
        runtime_config.bindings.len(),
        runtime_config.strategies.len()
    );

    // Create platform and run event loop
    let mut platform = Platform::new();

    if let Err(err) = platform
        .run(|event: KeyEvent, platform_handle: PlatformHandle| {
            handle_event(event, platform_handle, &runtime_config)
        })
        .await
    {
        eprintln!("error: {err:?}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Handle a key event from the platform
async fn handle_event(
    event: KeyEvent,
    platform: PlatformHandle,
    config: &RuntimeConfig,
) -> EventResponse {
    // TODO: Fast-path optimization - check static BOUND_KEYS set before crossing
    // async boundary to avoid channel overhead for unbound keys (~99% of key presses)

    // Check if this key has a binding - if not, pass through
    let Some(binding) = config.bindings.get(&event.key) else {
        return EventResponse::Passthrough;
    };
    let event = &event; // Reborrow for the rest of the function

    // Resolve the action based on window context
    let window = platform.get_active_window();
    let action = match &binding.action {
        ActionSpec::Simple(action) => action,
        ActionSpec::Conditional(rules) => {
            let mut resolved = None;
            for rule in rules {
                if rule.condition.is_empty() || rule.condition.window.matches(&window) {
                    resolved = Some(&rule.action);
                    break;
                }
            }
            match resolved {
                Some(action) => action,
                None => return EventResponse::Passthrough,
            }
        }
    };

    // Handle passthrough/block actions directly
    if matches!(action, Action::Passthrough) {
        return EventResponse::Passthrough;
    }
    if matches!(action, Action::Block) {
        return EventResponse::Block;
    }

    // TODO: For strategies that don't need async (direct action execution),
    // consider thread-local dispatch to avoid tokio scheduling overhead

    // If binding has a strategy, delegate to it
    if let Some(ref strategy_ref) = binding.strategy {
        let strategy_name = strategy_ref.value();
        if let Some(strategy) = config.strategies.get(strategy_name) {
            let ctx = StrategyContext::new(platform, action);
            let mut strategy_guard = strategy.lock().await;
            return strategy_guard.process(event, &ctx).await;
        } else {
            // This should not happen if validation is working correctly
            debug!(
                strategy = strategy_name,
                key = ?event.key,
                "strategy not found, falling through to direct execution"
            );
        }
    }

    // No strategy: execute action directly on key-down
    if event.down {
        debug!(key = ?event.key, ?action, "executing action directly");
        platform.execute(action);
    }
    EventResponse::Block
}
