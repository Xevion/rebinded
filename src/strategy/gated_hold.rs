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

use crate::config::Action;
use crate::key::{InputEvent, InputEventId};
use crate::platform::EventResponse;
use crate::strategy::{KeyStrategy, PlatformHandle, StrategyContext};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

/// Configuration for gated hold behavior
#[derive(Debug, Clone)]
pub struct GatedHoldConfig {
    /// How long the key must be held before first activation (ms)
    pub initial_hold_ms: u64,
    /// Window during which repeated presses activate immediately (ms)
    pub repeat_window_ms: u64,
    /// Events that divert the strategy to alternative actions while a key is held.
    /// When a divert event occurs while any key is in `Holding` or `Active` state,
    /// the key transitions to `Diverted` and the mapped action is executed.
    pub diverts: HashMap<InputEventId, Action>,
}

/// Tracks state for a single key
#[derive(Debug, Default)]
enum KeyState {
    /// No activity
    #[default]
    Idle,
    /// Key is held, waiting for hold threshold.
    /// Contains a cancel sender to abort the pending activation timer.
    Holding { cancel_tx: oneshot::Sender<()> },
    /// Key was activated and is being held
    Active,
    /// Key was diverted by a scroll or other event.
    /// The original action was either cancelled (from Holding) or released (from Active).
    /// The key is still physically held but the strategy is now handling divert actions.
    Diverted,
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
    /// Cached platform handle for executing divert actions
    /// Set on first key event, used for scroll events
    platform_handle: Option<PlatformHandle>,
    /// Channel to receive timer completion notifications
    /// When a hold timer fires, it sends the key name here so the strategy
    /// can transition the key from Holding to Active state
    timer_tx: mpsc::UnboundedSender<String>,
    timer_rx: mpsc::UnboundedReceiver<String>,
}

impl GatedHoldStrategy {
    /// Create a new gated hold strategy with the given configuration
    pub fn new(config: GatedHoldConfig) -> Self {
        let (timer_tx, timer_rx) = mpsc::unbounded_channel();
        Self {
            config,
            key_states: HashMap::new(),
            last_release: None,
            platform_handle: None,
            timer_tx,
            timer_rx,
        }
    }

    /// Check if any key is currently in a "held" state (Holding, Active, or Diverted)
    fn any_key_held(&self) -> bool {
        self.key_states.values().any(|s| {
            matches!(
                s,
                KeyState::Holding { .. } | KeyState::Active | KeyState::Diverted
            )
        })
    }

    /// Transition all held keys to Diverted state, cancelling timers and recording
    /// last_release as appropriate.
    fn divert_all_held_keys(&mut self) {
        let keys_to_divert: Vec<String> = self
            .key_states
            .iter()
            .filter(|(_, v)| matches!(v, KeyState::Holding { .. } | KeyState::Active))
            .map(|(k, _)| k.clone())
            .collect();

        for key_name in keys_to_divert {
            if let Some(state) = self.key_states.remove(&key_name) {
                match state {
                    KeyState::Holding { cancel_tx } => {
                        debug!(key = %key_name, "gated_hold: holding -> diverted (scroll)");
                        // Cancel the pending timer
                        let _ = cancel_tx.send(());
                    }
                    KeyState::Active => {
                        debug!(key = %key_name, "gated_hold: active -> diverted (scroll)");
                        // Record release time so repeat window is preserved
                        self.last_release = Some(Instant::now());
                    }
                    _ => {}
                }
                self.key_states.insert(key_name, KeyState::Diverted);
            }
        }
    }

    /// Handle a divert event (e.g., scroll while key is held)
    fn handle_divert(&mut self, event_id: &InputEventId) -> EventResponse {
        // Check if we have diverts configured for this event
        let Some(action) = self.config.diverts.get(event_id).cloned() else {
            return EventResponse::Passthrough;
        };

        // Check if any key is held (Holding, Active, or already Diverted)
        if !self.any_key_held() {
            // No keys held, pass through the event
            return EventResponse::Passthrough;
        }

        // Transition any Holding/Active keys to Diverted
        self.divert_all_held_keys();

        // Execute the divert action
        if let Some(handle) = &self.platform_handle {
            debug!(?action, "gated_hold: executing divert action");
            handle.execute(&action);
        }

        EventResponse::Block
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

    /// Process pending timer completion notifications
    ///
    /// When hold timers fire, they send the key name through the timer channel.
    /// This method drains the channel and transitions keys from Holding to Active.
    /// Should be called at the start of `process()` to ensure state is up-to-date.
    fn process_timer_completions(&mut self) {
        while let Ok(key_name) = self.timer_rx.try_recv() {
            // Check if key is still in Holding state
            // (it might have been released already, in which case we ignore the message)
            if let Some(KeyState::Holding { .. }) = self.key_states.get(&key_name) {
                debug!(key = %key_name, "gated_hold: holding -> active (timer completion)");
                self.key_states.insert(key_name, KeyState::Active);
            }
        }
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
                    let timer_tx = self.timer_tx.clone();
                    let key_name_owned = key_name.to_string();

                    tokio::spawn(async move {
                        tokio::select! {
                            _ = tokio::time::sleep(hold_duration) => {
                                // Hold threshold reached — execute action
                                platform_handle.execute(&action);
                                debug!("gated_hold: hold timer fired, action executed");

                                // Notify strategy that timer completed so it can transition to Active
                                let _ = timer_tx.send(key_name_owned);
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
            KeyState::Diverted => {
                // Already diverted, suppress repeated key-down events
                self.key_states
                    .insert(key_name.to_string(), KeyState::Diverted);
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
            KeyState::Diverted => {
                debug!(key = key_name, "gated_hold: diverted -> idle");
                // Don't record last_release here - it was already recorded when we
                // transitioned to Diverted (if coming from Active)
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
    fn subscriptions(&self) -> HashSet<InputEventId> {
        // Subscribe to all events that have diverts configured
        self.config.diverts.keys().cloned().collect()
    }

    async fn process(&mut self, event: &InputEvent, ctx: &StrategyContext) -> EventResponse {
        // Process any pending timer completions first
        self.process_timer_completions();

        // Cache the platform handle for use in divert actions
        if self.platform_handle.is_none() {
            self.platform_handle = Some(ctx.platform_handle());
        }

        match event {
            InputEvent::Key(key_event) => {
                let key_name = key_event.key.to_string();
                if key_event.down {
                    self.key_down(&key_name, ctx)
                } else {
                    self.key_up(&key_name)
                }
            }
            InputEvent::Scroll { up } => {
                let event_id = InputEventId::Scroll { up: *up };
                self.handle_divert(&event_id)
            }
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
            diverts: HashMap::new(),
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
            diverts: HashMap::new(),
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

    // ========================================================================
    // Divert State Transition Tests
    // ========================================================================

    fn config_with_diverts() -> GatedHoldConfig {
        let mut diverts = HashMap::new();
        diverts.insert(
            InputEventId::Scroll { up: true },
            crate::config::Action::VolumeUp,
        );
        diverts.insert(
            InputEventId::Scroll { up: false },
            crate::config::Action::VolumeDown,
        );
        GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 200,
            diverts,
        }
    }

    #[test]
    fn test_any_key_held_detects_holding_state() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());
        assert!(!strategy.any_key_held(), "should start with no keys held");

        // Create a dummy cancel channel for Holding state
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Holding { cancel_tx });

        assert!(strategy.any_key_held(), "should detect Holding state");
    }

    #[test]
    fn test_any_key_held_detects_active_state() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Active);

        assert!(strategy.any_key_held(), "should detect Active state");
    }

    #[test]
    fn test_any_key_held_detects_diverted_state() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Diverted);

        assert!(strategy.any_key_held(), "should detect Diverted state");
    }

    #[test]
    fn test_scroll_with_no_keys_held_passes_through() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        let event_id = InputEventId::Scroll { up: true };
        let response = strategy.handle_divert(&event_id);

        assert!(
            matches!(response, EventResponse::Passthrough),
            "scroll with no keys held should pass through"
        );
    }

    #[test]
    fn test_scroll_during_holding_transitions_to_diverted() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        // Set up key in Holding state
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Holding { cancel_tx });

        let event_id = InputEventId::Scroll { up: true };
        let response = strategy.handle_divert(&event_id);

        assert!(
            matches!(response, EventResponse::Block),
            "scroll during Holding should block"
        );
        assert!(
            matches!(strategy.key_states.get("f15"), Some(KeyState::Diverted)),
            "key should transition to Diverted state"
        );
        assert!(
            strategy.last_release.is_none(),
            "last_release should NOT be recorded when transitioning from Holding"
        );
    }

    #[test]
    fn test_scroll_during_active_transitions_to_diverted_and_records_release() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        // Set up key in Active state
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Active);
        assert!(
            strategy.last_release.is_none(),
            "precondition: no last_release"
        );

        let event_id = InputEventId::Scroll { up: true };
        let response = strategy.handle_divert(&event_id);

        assert!(
            matches!(response, EventResponse::Block),
            "scroll during Active should block"
        );
        assert!(
            matches!(strategy.key_states.get("f15"), Some(KeyState::Diverted)),
            "key should transition to Diverted state"
        );
        assert!(
            strategy.last_release.is_some(),
            "last_release SHOULD be recorded when transitioning from Active"
        );
    }

    #[test]
    fn test_scroll_during_diverted_stays_diverted() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        // Set up key in Diverted state
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Diverted);

        let event_id = InputEventId::Scroll { up: true };
        let response = strategy.handle_divert(&event_id);

        assert!(
            matches!(response, EventResponse::Block),
            "scroll during Diverted should block"
        );
        assert!(
            matches!(strategy.key_states.get("f15"), Some(KeyState::Diverted)),
            "key should remain in Diverted state"
        );
    }

    #[test]
    fn test_key_up_from_diverted_returns_to_idle() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        // Set up key in Diverted state
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Diverted);

        let response = strategy.key_up("f15");

        assert!(
            matches!(response, EventResponse::Block),
            "key up should block"
        );
        assert!(
            !strategy.key_states.contains_key("f15"),
            "key should be removed from map (Idle)"
        );
    }

    #[test]
    fn test_multiple_keys_all_transition_to_diverted() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        // Set up multiple keys in different held states
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Holding { cancel_tx });
        strategy
            .key_states
            .insert("f16".to_string(), KeyState::Active);

        let event_id = InputEventId::Scroll { up: true };
        let response = strategy.handle_divert(&event_id);

        assert!(matches!(response, EventResponse::Block), "should block");
        assert!(
            matches!(strategy.key_states.get("f15"), Some(KeyState::Diverted)),
            "f15 should transition to Diverted"
        );
        assert!(
            matches!(strategy.key_states.get("f16"), Some(KeyState::Diverted)),
            "f16 should transition to Diverted"
        );
    }

    #[test]
    fn test_repeat_window_preserved_after_divert_from_active() {
        let mut strategy = GatedHoldStrategy::new(GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 500, // Long window
            diverts: config_with_diverts().diverts,
        });

        // Set up key in Active state and divert
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Active);
        let event_id = InputEventId::Scroll { up: true };
        strategy.handle_divert(&event_id);

        // Release the key
        strategy.key_up("f15");

        // Gate should still be open (we're in repeat window)
        assert!(
            strategy.is_gate_open(),
            "gate should be open after divert from Active"
        );
    }

    #[test]
    fn test_subscriptions_returns_divert_keys() {
        let strategy = GatedHoldStrategy::new(config_with_diverts());
        let subs = strategy.subscriptions();

        assert!(subs.len() == 2, "should have 2 subscriptions");
        assert!(
            subs.contains(&InputEventId::Scroll { up: true }),
            "should subscribe to scroll_up"
        );
        assert!(
            subs.contains(&InputEventId::Scroll { up: false }),
            "should subscribe to scroll_down"
        );
    }

    #[test]
    fn test_subscriptions_empty_without_diverts() {
        let strategy = GatedHoldStrategy::new(test_config()); // No diverts
        let subs = strategy.subscriptions();

        assert!(
            subs.is_empty(),
            "should have no subscriptions without diverts"
        );
    }

    #[test]
    fn test_unrecognized_divert_event_passes_through() {
        let mut strategy = GatedHoldStrategy::new(config_with_diverts());

        // Set up key in Active state
        strategy
            .key_states
            .insert("f15".to_string(), KeyState::Active);

        // Try to divert with an event that's not in the diverts map
        let unknown_event = InputEventId::Key(crate::key::KeyCode::new(0x99));
        let response = strategy.handle_divert(&unknown_event);

        assert!(
            matches!(response, EventResponse::Passthrough),
            "unrecognized divert event should pass through"
        );
        assert!(
            matches!(strategy.key_states.get("f15"), Some(KeyState::Active)),
            "key should remain in Active state"
        );
    }

    // ========================================================================
    // Timer Completion Tests
    // ========================================================================

    #[tokio::test]
    async fn test_timer_completion_transitions_to_active() {
        use crate::config::Action;
        use crate::key::{KeyCode, KeyEvent};
        use crate::platform::MockPlatform;
        use crate::strategy::{PlatformHandle, StrategyContext};
        use std::sync::Arc;

        let config = GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 200,
            diverts: HashMap::new(),
        };
        let mut strategy = GatedHoldStrategy::new(config);

        // Create mock platform and context
        let platform = Arc::new(MockPlatform::new());
        let platform_handle = unsafe { PlatformHandle::from_mock(&platform) };
        let action = Action::MediaNext;
        let ctx = StrategyContext::new(platform_handle, &action);

        // Press key
        let key_event = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), true));
        let response = strategy.process(&key_event, &ctx).await;
        assert!(matches!(response, EventResponse::Block));

        // Key should be in Holding state
        assert!(matches!(
            strategy.key_states.get("KEY_ESC"),
            Some(KeyState::Holding { .. })
        ));

        // Wait for timer to fire (slightly longer than initial_hold_ms)
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Process timer completions by calling process with a dummy event
        // (In real usage, this happens automatically on the next event)
        strategy.process_timer_completions();

        // Key should now be in Active state
        assert!(
            matches!(strategy.key_states.get("KEY_ESC"), Some(KeyState::Active)),
            "key should transition to Active after timer fires"
        );

        // Release key
        let key_up = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), false));
        strategy.process(&key_up, &ctx).await;

        // last_release should be recorded
        assert!(
            strategy.last_release.is_some(),
            "last_release should be recorded when releasing Active key"
        );

        // Verify that MediaNext was actually executed
        platform.assert_media_sent(crate::platform::MediaCommand::Next);
        platform.assert_call_count(1);
    }

    #[tokio::test]
    async fn test_repeat_window_after_timer_activation() {
        use crate::config::Action;
        use crate::key::{KeyCode, KeyEvent};
        use crate::platform::MockPlatform;
        use crate::strategy::{PlatformHandle, StrategyContext};
        use std::sync::Arc;

        let config = GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 500, // Long window for testing
            diverts: HashMap::new(),
        };
        let mut strategy = GatedHoldStrategy::new(config);

        let platform = Arc::new(MockPlatform::new());
        let platform_handle = unsafe { PlatformHandle::from_mock(&platform) };
        let action = Action::MediaNext;
        let ctx = StrategyContext::new(platform_handle, &action);

        // First press: hold until timer fires
        let key_down = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), true));
        strategy.process(&key_down, &ctx).await;

        // Wait for timer
        tokio::time::sleep(Duration::from_millis(60)).await;
        strategy.process_timer_completions();

        // Release
        let key_up = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), false));
        strategy.process(&key_up, &ctx).await;

        // Gate should be open
        assert!(
            strategy.is_gate_open(),
            "gate should be open after release within repeat window"
        );

        // Quick second press (within repeat window)
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = strategy.process(&key_down, &ctx).await;

        // Should activate immediately without waiting for hold timer
        assert!(
            matches!(strategy.key_states.get("KEY_ESC"), Some(KeyState::Active)),
            "second press should activate immediately (gate open)"
        );
        assert!(matches!(response, EventResponse::Block));
    }

    #[tokio::test]
    async fn test_timer_completion_after_early_release() {
        use crate::config::Action;
        use crate::key::{KeyCode, KeyEvent};
        use crate::platform::MockPlatform;
        use crate::strategy::{PlatformHandle, StrategyContext};
        use std::sync::Arc;

        let config = GatedHoldConfig {
            initial_hold_ms: 100,
            repeat_window_ms: 200,
            diverts: HashMap::new(),
        };
        let mut strategy = GatedHoldStrategy::new(config);

        let platform = Arc::new(MockPlatform::new());
        let platform_handle = unsafe { PlatformHandle::from_mock(&platform) };
        let action = Action::MediaNext;
        let ctx = StrategyContext::new(platform_handle, &action);

        // Press key
        let key_down = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), true));
        strategy.process(&key_down, &ctx).await;

        // Release before timer fires
        tokio::time::sleep(Duration::from_millis(20)).await;
        let key_up = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), false));
        strategy.process(&key_up, &ctx).await;

        // Key should be Idle
        assert!(
            !strategy.key_states.contains_key("KEY_ESC"),
            "key should be Idle after early release"
        );

        // Wait for timer to fire (it will, but the message should be ignored)
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Process timer completion
        strategy.process_timer_completions();

        // Key should still be Idle (not transitioned to Active)
        assert!(
            !strategy.key_states.contains_key("KEY_ESC"),
            "key should remain Idle even after timer message arrives"
        );
    }

    #[tokio::test]
    async fn test_multiple_keys_share_gate_after_timer() {
        use crate::config::Action;
        use crate::key::{KeyCode, KeyEvent};
        use crate::platform::MockPlatform;
        use crate::strategy::{PlatformHandle, StrategyContext};
        use std::sync::Arc;

        let config = GatedHoldConfig {
            initial_hold_ms: 50,
            repeat_window_ms: 500,
            diverts: HashMap::new(),
        };
        let mut strategy = GatedHoldStrategy::new(config);

        let platform = Arc::new(MockPlatform::new());
        let platform_handle = unsafe { PlatformHandle::from_mock(&platform) };
        let action = Action::MediaNext;
        let ctx = StrategyContext::new(platform_handle, &action);

        // Press key1, wait for timer, release
        let key1_down = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), true));
        strategy.process(&key1_down, &ctx).await;

        tokio::time::sleep(Duration::from_millis(60)).await;
        strategy.process_timer_completions();

        let key1_up = InputEvent::Key(KeyEvent::new(KeyCode::new(0x1), false));
        strategy.process(&key1_up, &ctx).await;

        // Gate should be open
        assert!(strategy.is_gate_open());

        // Press different key2 quickly
        let key2_down = InputEvent::Key(KeyEvent::new(KeyCode::new(0x2), true));
        let response = strategy.process(&key2_down, &ctx).await;

        // Key2 should activate immediately (shared gate is open)
        assert!(
            matches!(strategy.key_states.get("KEY_1"), Some(KeyState::Active)),
            "key2 should activate immediately due to shared gate"
        );
        assert!(matches!(response, EventResponse::Block));

        // Verify both key presses executed the action (first after timer, second immediately)
        platform.assert_call_count(2);
    }

    #[tokio::test]
    async fn test_mock_platform_records_actions() {
        use crate::config::Action;
        use crate::key::{KeyCode, KeyEvent};
        use crate::platform::{MediaCommand, MockPlatform};
        use crate::strategy::{PlatformHandle, StrategyContext};
        use std::sync::Arc;

        let platform = Arc::new(MockPlatform::new());
        let platform_handle = unsafe { PlatformHandle::from_mock(&platform) };

        // Test that executing actions records them
        let action = Action::MediaNext;
        let ctx = StrategyContext::new(platform_handle, &action);

        // Initially no calls
        platform.assert_no_calls();

        // Execute action
        ctx.execute();

        // Should have recorded the media command
        platform.assert_media_sent(MediaCommand::Next);
        platform.assert_call_count(1);

        // Execute another action
        let action2 = Action::VolumeUp;
        action2.execute(&*platform);

        // Should have 2 calls now
        platform.assert_call_count(2);

        // Clear and verify
        platform.clear_calls();
        platform.assert_no_calls();
    }
}
