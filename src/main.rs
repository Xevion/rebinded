mod config;
mod key;
mod platform;
mod strategy;

use clap::Parser;
use config::{Action, RuntimeConfig};
use key::InputEvent;
use platform::{EventResponse, Platform, PlatformInterface};
use std::path::PathBuf;
use std::process::ExitCode;
use strategy::{PlatformHandle, StrategyContext};
use tracing::{Level, debug, info, trace};
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

    let (config, runtime_config) = match config::load(&config_path).await {
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
        .run(|event: InputEvent, platform_handle: PlatformHandle| {
            handle_event(event, platform_handle, &runtime_config)
        })
        .await
    {
        eprintln!("error: {err:?}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Handle an input event from the platform
async fn handle_event(
    event: InputEvent,
    platform: PlatformHandle,
    config: &RuntimeConfig,
) -> EventResponse {
    let event_id = event.id();

    // Check if any strategy is subscribed to this event
    if let Some(strategy_names) = config.subscriptions.get(&event_id) {
        trace!(
            ?event_id,
            ?strategy_names,
            "routing to subscribed strategies"
        );

        // Route to each subscribed strategy
        // If any strategy blocks, return Block; otherwise Passthrough
        for strategy_name in strategy_names {
            if let Some(strategy) = config.strategies.get(strategy_name) {
                // For subscribed events, we use a dummy action since the strategy
                // will use its own divert actions
                let ctx = StrategyContext::new(platform, &Action::Block);
                let mut strategy_guard = strategy.lock().await;
                let response = strategy_guard.process(&event, &ctx).await;

                if response == EventResponse::Block {
                    return EventResponse::Block;
                }
            }
        }

        // No strategy blocked, check if this is a key event that also has bindings
        // (fall through to normal handling below)
    }

    // For scroll events with no subscriptions, pass through
    let key_event = match &event {
        InputEvent::Key(key_event) => key_event,
        InputEvent::Scroll { .. } => {
            return EventResponse::Passthrough;
        }
    };

    // TODO: Fast-path optimization - check static BOUND_KEYS set before crossing
    // async boundary to avoid channel overhead for unbound keys (~99% of key presses)

    // Check if this key has a binding - if not, pass through
    let Some(binding) = config.bindings.get(&key_event.key) else {
        return EventResponse::Passthrough;
    };

    // Resolve the action based on window context
    let window = platform.get_active_window();
    let Some(action) = config.resolve_action(key_event.key, &window) else {
        return EventResponse::Passthrough;
    };

    // Handle passthrough/block actions directly
    if let Some(response) = action.as_response() {
        return response;
    }

    // TODO: For strategies that don't need async (direct action execution),
    // consider thread-local dispatch to avoid tokio scheduling overhead

    // If binding has a strategy, delegate to it
    if let Some(ref strategy_ref) = binding.strategy {
        let strategy_name = strategy_ref.value();
        if let Some(strategy) = config.strategies.get(strategy_name) {
            let ctx = StrategyContext::new(platform, action);
            let mut strategy_guard = strategy.lock().await;
            return strategy_guard.process(&event, &ctx).await;
        } else {
            // This should not happen if validation is working correctly
            debug!(
                strategy = strategy_name,
                key = ?key_event.key,
                "strategy not found, falling through to direct execution"
            );
        }
    }

    // No strategy: execute action directly on key-down
    if key_event.down {
        debug!(key = ?key_event.key, ?action, "executing action directly");
        platform.execute(action);
    }
    EventResponse::Block
}
