//! Gated hold strategy
//!
//! Implements a two-phase activation model:
//! 1. **Initial gate**: Key must be held for `initial_hold_ms` before first activation
//! 2. **Repeat window**: After activation, subsequent presses activate immediately
//!    for `repeat_window_ms`
//!
//! This prevents accidental activation (e.g., bumping scroll wheel tilt) while
//! allowing intentional rapid activation (e.g., skipping multiple tracks).
//!
//! Keys sharing the same `GatedHoldStrategy` instance share gate state — if one key
//! opens the gate, sibling keys can activate immediately.

use crate::key::KeyEvent;
use crate::platform::EventResponse;
use crate::strategy::{KeyStrategy, StrategyContext};
use async_trait::async_trait;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::debug;

/// Configuration for gated hold behavior
#[derive(Debug, Clone)]
pub struct GatedHoldConfig {
    /// How long the key must be held before first activation (ms)
    pub initial_hold_ms: u64,
    /// Window during which repeated presses activate immediately (ms)
    pub repeat_window_ms: u64,
}

/// Tracks state for a single key
#[derive(Debug)]
enum KeyState {
    /// No activity
    Idle,
    /// Key is held, waiting for hold threshold.
    /// Contains a cancel sender to abort the pending activation timer.
    Holding { cancel_tx: oneshot::Sender<()> },
    /// Key was activated and is being held
    Active,
}

impl Default for KeyState {
    fn default() -> Self {
        Self::Idle
    }
}

/// Gated hold strategy implementation
///
/// Multiple keys can share an instance to share gate state. When any key in the
/// group activates, the gate opens for all keys in that group.
pub struct GatedHoldStrategy {
    config: GatedHoldConfig,
    /// Per-key state
    key_states: HashMap<String, KeyState>,
    /// When a key was last released (for repeat window)
    last_release: Option<Instant>,
}

impl GatedHoldStrategy {
    /// Create a new gated hold strategy with the given configuration
    pub fn new(config: GatedHoldConfig) -> Self {
        Self {
            config,
            key_states: HashMap::new(),
            last_release: None,
        }
    }

    /// Check if the gate is currently open
    ///
    /// Gate is open if:
    /// - Any key is currently Active, OR
    /// - We're within repeat_window_ms of the last release
    fn is_gate_open(&self) -> bool {
        // Check if any key is active
        let any_active = self
            .key_states
            .values()
            .any(|s| matches!(s, KeyState::Active));
        if any_active {
            return true;
        }

        // Check if we're in the repeat window
        if let Some(last) = self.last_release {
            let repeat_window = Duration::from_millis(self.config.repeat_window_ms);
            if last.elapsed() < repeat_window {
                return true;
            }
        }

        false
    }

    /// Handle key-down event
    fn key_down(&mut self, key_name: &str, ctx: &StrategyContext) -> EventResponse {
        let gate_open = self.is_gate_open();

        // Get current state, defaulting to Idle
        let current_state = self.key_states.remove(key_name).unwrap_or(KeyState::Idle);

        match current_state {
            KeyState::Idle => {
                if gate_open {
                    debug!(key = key_name, "gated_hold: idle -> active (gate open)");
                    self.key_states
                        .insert(key_name.to_string(), KeyState::Active);
                    ctx.execute();
                    EventResponse::Block
                } else {
                    debug!(key = key_name, "gated_hold: idle -> holding");

                    // Spawn timer for delayed activation
                    let (cancel_tx, cancel_rx) = oneshot::channel();
                    let hold_duration = Duration::from_millis(self.config.initial_hold_ms);

                    // Clone what we need for the spawned task
                    let action = ctx.action().clone();
                    let platform_handle = ctx.platform_handle();

                    tokio::spawn(async move {
                        tokio::select! {
                            _ = tokio::time::sleep(hold_duration) => {
                                // Hold threshold reached — execute action
                                platform_handle.execute(&action);
                                debug!("gated_hold: hold timer fired, action executed");
                            }
                            _ = cancel_rx => {
                                // Cancelled (key released early)
                                debug!("gated_hold: hold timer cancelled");
                            }
                        }
                    });

                    self.key_states
                        .insert(key_name.to_string(), KeyState::Holding { cancel_tx });
                    EventResponse::Block
                }
            }
            KeyState::Holding { cancel_tx } => {
                // Still holding, put the state back
                // This handles OS key repeat events while holding
                self.key_states
                    .insert(key_name.to_string(), KeyState::Holding { cancel_tx });
                EventResponse::Block
            }
            KeyState::Active => {
                // Already active, suppress repeated key-down events
                self.key_states
                    .insert(key_name.to_string(), KeyState::Active);
                EventResponse::Block
            }
        }
    }

    /// Handle key-up event
    pub(crate) fn key_up(&mut self, key_name: &str) -> EventResponse {
        let current_state = self.key_states.remove(key_name).unwrap_or(KeyState::Idle);

        match current_state {
            KeyState::Holding { cancel_tx } => {
                debug!(key = key_name, "gated_hold: holding -> idle (cancelled)");
                // Cancel the pending timer
                let _ = cancel_tx.send(());
                // Don't reinsert - absence from map means Idle
            }
            KeyState::Active => {
                debug!(key = key_name, "gated_hold: active -> idle");
                // Record release time for repeat window
                self.last_release = Some(Instant::now());
                // Don't reinsert - absence from map means Idle
            }
            KeyState::Idle => {
                // Already idle, nothing to do
            }
        }

        // Always block key-up for keys we're managing
        EventResponse::Block
    }
}

#[async_trait]
impl KeyStrategy for GatedHoldStrategy {
    async fn process(&mut self, event: &KeyEvent, ctx: &StrategyContext) -> EventResponse {
        let key_name = event.key.to_string();
        if event.down {
            self.key_down(&key_name, ctx)
        } else {
            self.key_up(&key_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> GatedHoldConfig {
        GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 200,
        }
    }

    #[test]
    fn test_gate_closed_initially() {
        let strategy = GatedHoldStrategy::new(test_config());
        assert!(!strategy.is_gate_open());
    }

    #[test]
    fn test_gate_open_with_active_key() {
        let mut strategy = GatedHoldStrategy::new(test_config());
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Active);
        assert!(strategy.is_gate_open());
    }

    #[test]
    fn test_gate_open_in_repeat_window() {
        let mut strategy = GatedHoldStrategy::new(test_config());
        strategy.last_release = Some(Instant::now());
        assert!(strategy.is_gate_open());
    }

    #[test]
    fn test_gate_closed_after_repeat_window() {
        let mut strategy = GatedHoldStrategy::new(GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 10, // Short window for testing
        });
        strategy.last_release = Some(Instant::now() - Duration::from_millis(20));
        assert!(!strategy.is_gate_open());
    }

    #[test]
    fn test_key_up_removes_active_state_from_map() {
        let mut strategy = GatedHoldStrategy::new(test_config());

        // Set up key in Active state
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Active);

        // Call the actual key_up method
        let response = strategy.key_up("f15");

        assert!(matches!(response, EventResponse::Block));
        assert!(
            !strategy.key_states.contains_key("f15"),
            "key_states should not contain released key"
        );
    }

    #[test]
    fn test_key_up_does_not_grow_map_unbounded() {
        let mut strategy = GatedHoldStrategy::new(test_config());

        // Simulate multiple keys going through Active -> release cycle
        for key in ["f15", "f16", "f17", "f18"] {
            strategy
                .key_states
                .insert(key.to_string(), KeyState::Active);
            strategy.key_up(key);
        }

        // After all keys are released, map should be empty
        assert!(
            strategy.key_states.is_empty(),
            "key_states should be empty after all keys released, but has {} entries",
            strategy.key_states.len()
        );
    }
}
