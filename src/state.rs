//! Debounce state machine
//!
//! Implements the two-phase debounce logic:
//! 1. Initial gate: Key must be held for `initial_hold_ms` before first activation
//! 2. Repeat window: After activation, subsequent presses within `repeat_window_ms` activate immediately
//!
//! State transitions:
//! ```text
//!                    key_down
//!     ┌─────────────────────────────────────┐
//!     │                                     ▼
//!   Idle ──key_down──▶ Holding ──timeout──▶ Active ──key_up──▶ Cooldown
//!     ▲                   │                   │                   │
//!     │                   │ key_up            │ key_down          │ timeout
//!     │                   │ (cancel)          │ (repeat)          │
//!     │                   ▼                   ▼                   │
//!     └───────────────── Idle ◀────────────────────────────────────┘
//! ```

use crate::config::DebounceProfile;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::debug;

/// Tracks debounce state for a single key
#[derive(Debug)]
enum KeyState {
    /// No activity, waiting for key press
    Idle,
    /// Key is held, waiting for initial_hold_ms to activate
    Holding { since: Instant },
    /// Key was activated, in repeat window
    Active { last_activation: Instant },
    /// Key released after activation, in cooldown before returning to idle
    Cooldown { since: Instant },
}

/// Result of processing a key event through the debounce state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceResult {
    /// Action should fire
    Activate,
    /// Action should not fire (still waiting, or in cooldown)
    Suppress,
    /// Key should pass through unchanged (no debounce configured)
    Passthrough,
}

/// Manages debounce state for multiple keys
pub struct DebounceManager {
    states: HashMap<String, KeyState>,
    profiles: HashMap<String, DebounceProfile>,
    /// Maps key names to their debounce profile names
    key_profiles: HashMap<String, String>,
}

impl DebounceManager {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
            profiles: HashMap::new(),
            key_profiles: HashMap::new(),
        }
    }

    /// Register a debounce profile
    pub fn add_profile(&mut self, name: String, profile: DebounceProfile) {
        self.profiles.insert(name, profile);
    }

    /// Associate a key with a debounce profile
    pub fn set_key_profile(&mut self, key: String, profile_name: String) {
        self.key_profiles.insert(key, profile_name);
    }

    /// Process a key-down event
    pub fn key_down(&mut self, key: &str) -> DebounceResult {
        let Some(profile_name) = self.key_profiles.get(key) else {
            return DebounceResult::Passthrough;
        };
        let Some(profile) = self.profiles.get(profile_name) else {
            return DebounceResult::Passthrough;
        };

        let now = Instant::now();
        let initial_hold = Duration::from_millis(profile.initial_hold_ms);
        let repeat_window = Duration::from_millis(profile.repeat_window_ms);

        let state = self.states.entry(key.to_string()).or_insert(KeyState::Idle);

        match state {
            KeyState::Idle => {
                debug!(key, "debounce: idle -> holding");
                *state = KeyState::Holding { since: now };
                DebounceResult::Suppress
            }
            KeyState::Holding { since } => {
                if now.duration_since(*since) >= initial_hold {
                    debug!(key, "debounce: holding -> active (initial hold complete)");
                    *state = KeyState::Active {
                        last_activation: now,
                    };
                    DebounceResult::Activate
                } else {
                    DebounceResult::Suppress
                }
            }
            KeyState::Active { last_activation } => {
                // Rapid repeat while in active state
                debug!(key, "debounce: active repeat");
                *last_activation = now;
                DebounceResult::Activate
            }
            KeyState::Cooldown { since } => {
                if now.duration_since(*since) < repeat_window {
                    // Still in repeat window, activate immediately
                    debug!(key, "debounce: cooldown -> active (repeat window)");
                    *state = KeyState::Active {
                        last_activation: now,
                    };
                    DebounceResult::Activate
                } else {
                    // Repeat window expired, start fresh
                    debug!(key, "debounce: cooldown -> holding (window expired)");
                    *state = KeyState::Holding { since: now };
                    DebounceResult::Suppress
                }
            }
        }
    }

    /// Process a key-up event
    pub fn key_up(&mut self, key: &str) -> DebounceResult {
        if !self.key_profiles.contains_key(key) {
            return DebounceResult::Passthrough;
        }

        let now = Instant::now();
        let state = self.states.entry(key.to_string()).or_insert(KeyState::Idle);

        match state {
            KeyState::Holding { .. } => {
                // Released before activation threshold
                debug!(key, "debounce: holding -> idle (cancelled)");
                *state = KeyState::Idle;
            }
            KeyState::Active { .. } => {
                // Enter cooldown for potential rapid repeats
                debug!(key, "debounce: active -> cooldown");
                *state = KeyState::Cooldown { since: now };
            }
            _ => {}
        }

        // Key-up events are always suppressed for debounced keys
        DebounceResult::Suppress
    }

    /// Called periodically to check for state transitions based on time
    /// Returns keys that should activate due to hold timeout
    pub fn tick(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut to_activate = Vec::new();

        for (key, state) in &mut self.states {
            if let KeyState::Holding { since } = state {
                let profile_name = match self.key_profiles.get(key) {
                    Some(p) => p,
                    None => continue,
                };
                let profile = match self.profiles.get(profile_name) {
                    Some(p) => p,
                    None => continue,
                };

                let initial_hold = Duration::from_millis(profile.initial_hold_ms);
                if now.duration_since(*since) >= initial_hold {
                    debug!(key, "debounce: tick -> activate");
                    *state = KeyState::Active {
                        last_activation: now,
                    };
                    to_activate.push(key.clone());
                }
            }
        }

        to_activate
    }

    /// Clean up expired cooldown states
    pub fn cleanup(&mut self) {
        let now = Instant::now();

        self.states.retain(|key, state| {
            if let KeyState::Cooldown { since } = state {
                let profile_name = match self.key_profiles.get(key) {
                    Some(p) => p,
                    None => return false,
                };
                let profile = match self.profiles.get(profile_name) {
                    Some(p) => p,
                    None => return false,
                };

                let repeat_window = Duration::from_millis(profile.repeat_window_ms);
                if now.duration_since(*since) >= repeat_window {
                    debug!(key, "debounce: cooldown expired, removing state");
                    return false;
                }
            }
            true
        });
    }
}

impl Default for DebounceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::assert;
    use std::thread::sleep;

    fn test_profile() -> DebounceProfile {
        DebounceProfile {
            initial_hold_ms: 50,
            repeat_window_ms: 200,
        }
    }

    #[test]
    fn test_passthrough_unconfigured_key() {
        let mut manager = DebounceManager::new();
        assert!(manager.key_down("f13") == DebounceResult::Passthrough);
    }

    #[test]
    fn test_suppress_before_threshold() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());

        // First press should be suppressed
        assert!(manager.key_down("f15") == DebounceResult::Suppress);

        // Immediate second press should still be suppressed
        assert!(manager.key_down("f15") == DebounceResult::Suppress);
    }

    #[test]
    fn test_activate_after_threshold() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());

        // First press
        assert!(manager.key_down("f15") == DebounceResult::Suppress);

        // Wait for threshold
        sleep(Duration::from_millis(60));

        // Should activate now
        assert!(manager.key_down("f15") == DebounceResult::Activate);
    }

    #[test]
    fn test_repeat_in_window() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());

        // Activate
        manager.key_down("f15");
        sleep(Duration::from_millis(60));
        assert!(manager.key_down("f15") == DebounceResult::Activate);

        // Release
        manager.key_up("f15");

        // Quick re-press should activate immediately
        assert!(manager.key_down("f15") == DebounceResult::Activate);
    }

    #[test]
    fn test_repeat_window_expires() {
        let mut manager = DebounceManager::new();
        manager.add_profile(
            "scroll".to_string(),
            DebounceProfile {
                initial_hold_ms: 20,
                repeat_window_ms: 50,
            },
        );
        manager.set_key_profile("f15".to_string(), "scroll".to_string());

        // Activate
        manager.key_down("f15");
        sleep(Duration::from_millis(30));
        assert!(manager.key_down("f15") == DebounceResult::Activate);
        manager.key_up("f15");

        // Wait for repeat window to expire
        sleep(Duration::from_millis(60));

        // Should need to hold again
        assert!(manager.key_down("f15") == DebounceResult::Suppress);
    }
}
