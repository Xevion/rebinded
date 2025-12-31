//! Debounce state machine
//!
//! Implements the two-phase debounce logic with shared group state:
//! 1. Initial gate: Key must be held for `initial_hold_ms` before first activation
//! 2. Repeat window: After activation, ANY key in the same group activates immediately
//!
//! Keys sharing the same debounce profile share gate state. For example, scroll left/right
//! (F15/F16) both use the "scroll" profile, so holding F15 unlocks the gate for F16 too.

use crate::config::DebounceProfile;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::debug;

/// Tracks debounce state for a single key
#[derive(Debug, Clone)]
enum KeyState {
    /// No activity, waiting for key press
    Idle,
    /// Key is held, waiting for initial_hold_ms to activate
    Holding { since: Instant },
    /// Key was activated and is being held
    Active,
}

/// Tracks shared state for a debounce group (all keys sharing a profile)
/// The gate is "open" if any key is Active OR we're within repeat_window of last release
#[derive(Debug, Clone, Default)]
struct GroupState {
    /// When a key in this group was last released (for repeat window)
    last_release: Option<Instant>,
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

/// Manages debounce state for multiple keys with shared group state
pub struct DebounceManager {
    /// Per-key state (holding, active, etc.)
    key_states: HashMap<String, KeyState>,
    /// Per-group state (gate open/closed, last activation time)
    group_states: HashMap<String, GroupState>,
    /// Debounce profiles by name
    profiles: HashMap<String, DebounceProfile>,
    /// Maps key names to their debounce profile/group names
    key_profiles: HashMap<String, String>,
}

impl DebounceManager {
    pub fn new() -> Self {
        Self {
            key_states: HashMap::new(),
            group_states: HashMap::new(),
            profiles: HashMap::new(),
            key_profiles: HashMap::new(),
        }
    }

    /// Register a debounce profile
    pub fn add_profile(&mut self, name: String, profile: DebounceProfile) {
        self.profiles.insert(name, profile);
    }

    /// Associate a key with a debounce profile (group)
    pub fn set_key_profile(&mut self, key: String, profile_name: String) {
        self.key_profiles.insert(key, profile_name);
    }

    /// Check if a group's gate is open
    /// Gate is open if: any key in group is Active, OR within repeat_window of last release
    fn is_gate_open(&self, group_name: &str, profile: &DebounceProfile) -> bool {
        // Check if any key in this group is currently active
        let any_active = self.key_states.iter().any(|(k, s)| {
            matches!(s, KeyState::Active) && self.key_profiles.get(k).map(|s| s.as_str()) == Some(group_name)
        });
        if any_active {
            return true;
        }

        // Check if we're in the repeat window after last release
        if let Some(group) = self.group_states.get(group_name) {
            if let Some(last) = group.last_release {
                let repeat_window = Duration::from_millis(profile.repeat_window_ms);
                if last.elapsed() < repeat_window {
                    return true;
                }
            }
        }

        false
    }

    /// Process a key-down event
    pub fn key_down(&mut self, key: &str) -> DebounceResult {
        let Some(group_name) = self.key_profiles.get(key).cloned() else {
            return DebounceResult::Passthrough;
        };
        let Some(profile) = self.profiles.get(&group_name).cloned() else {
            return DebounceResult::Passthrough;
        };

        let now = Instant::now();
        let initial_hold = Duration::from_millis(profile.initial_hold_ms);

        // Check gate status before borrowing key_states mutably
        let gate_open = self.is_gate_open(&group_name, &profile);

        // Get current key state
        let current_state = self
            .key_states
            .get(key)
            .cloned()
            .unwrap_or(KeyState::Idle);

        match current_state {
            KeyState::Idle => {
                if gate_open {
                    debug!(key, group = %group_name, "debounce: idle -> active (gate open)");
                    self.key_states.insert(key.to_string(), KeyState::Active);
                    DebounceResult::Activate
                } else {
                    debug!(key, group = %group_name, "debounce: idle -> holding");
                    self.key_states
                        .insert(key.to_string(), KeyState::Holding { since: now });
                    DebounceResult::Suppress
                }
            }
            KeyState::Holding { since } => {
                if gate_open {
                    debug!(key, group = %group_name, "debounce: holding -> active (gate open)");
                    self.key_states.insert(key.to_string(), KeyState::Active);
                    DebounceResult::Activate
                } else if now.duration_since(since) >= initial_hold {
                    debug!(key, group = %group_name, "debounce: holding -> active (hold complete)");
                    self.key_states.insert(key.to_string(), KeyState::Active);
                    DebounceResult::Activate
                } else {
                    DebounceResult::Suppress
                }
            }
            KeyState::Active => {
                // Key is being held down, suppress repeated key-down events
                DebounceResult::Suppress
            }
        }
    }

    /// Process a key-up event
    pub fn key_up(&mut self, key: &str) -> DebounceResult {
        let Some(group_name) = self.key_profiles.get(key).cloned() else {
            return DebounceResult::Passthrough;
        };

        let now = Instant::now();
        let key_state = self
            .key_states
            .entry(key.to_string())
            .or_insert(KeyState::Idle);

        match key_state {
            KeyState::Holding { .. } => {
                debug!(key, group = %group_name, "debounce: holding -> idle (cancelled)");
                *key_state = KeyState::Idle;
            }
            KeyState::Active => {
                debug!(key, group = %group_name, "debounce: active -> idle");
                *key_state = KeyState::Idle;

                // Record release time for repeat window
                let group = self.group_states.entry(group_name).or_default();
                group.last_release = Some(now);
            }
            KeyState::Idle => {}
        }

        DebounceResult::Suppress
    }

    /// Called periodically to check for state transitions based on time
    /// Returns keys that should activate due to hold timeout
    pub fn tick(&mut self) -> Vec<String> {
        let now = Instant::now();
        let mut to_activate = Vec::new();

        // Collect keys that need activation
        let keys_to_check: Vec<_> = self
            .key_states
            .iter()
            .filter_map(|(key, state)| {
                if let KeyState::Holding { since } = state {
                    Some((key.clone(), *since))
                } else {
                    None
                }
            })
            .collect();

        for (key, since) in keys_to_check {
            let Some(group_name) = self.key_profiles.get(&key) else {
                continue;
            };
            let Some(profile) = self.profiles.get(group_name) else {
                continue;
            };

            let initial_hold = Duration::from_millis(profile.initial_hold_ms);
            if now.duration_since(since) >= initial_hold {
                debug!(key, "debounce: tick -> activate");

                if let Some(state) = self.key_states.get_mut(&key) {
                    *state = KeyState::Active;
                }

                to_activate.push(key);
            }
        }

        to_activate
    }

    /// Clean up expired group states
    pub fn cleanup(&mut self) {
        let now = Instant::now();

        for (group_name, group) in &mut self.group_states {
            if let Some(last) = group.last_release {
                let Some(profile) = self.profiles.get(group_name) else {
                    continue;
                };
                let repeat_window = Duration::from_millis(profile.repeat_window_ms);
                if now.duration_since(last) >= repeat_window {
                    debug!(group = %group_name, "debounce: repeat window expired");
                    group.last_release = None;
                }
            }
        }
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
    fn test_repeat_in_window_same_key() {
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

    #[test]
    fn test_shared_group_gate_opens_for_sibling() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());
        manager.set_key_profile("f16".to_string(), "scroll".to_string());

        // F15 starts holding
        assert!(manager.key_down("f15") == DebounceResult::Suppress);

        // Wait for threshold, F15 activates and opens gate
        sleep(Duration::from_millis(60));
        assert!(manager.key_down("f15") == DebounceResult::Activate);

        // F16 should activate immediately (gate is open)
        assert!(manager.key_down("f16") == DebounceResult::Activate);
    }

    #[test]
    fn test_shared_group_repeat_window_works_for_sibling() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());
        manager.set_key_profile("f16".to_string(), "scroll".to_string());

        // F15 activates
        manager.key_down("f15");
        sleep(Duration::from_millis(60));
        assert!(manager.key_down("f15") == DebounceResult::Activate);
        manager.key_up("f15");

        // F16 should activate immediately (in repeat window)
        assert!(manager.key_down("f16") == DebounceResult::Activate);
    }

    #[test]
    fn test_shared_group_both_keys_active_keeps_gate_open() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());
        manager.set_key_profile("f16".to_string(), "scroll".to_string());

        // F15 activates
        manager.key_down("f15");
        sleep(Duration::from_millis(60));
        assert!(manager.key_down("f15") == DebounceResult::Activate);

        // F16 activates while F15 still held
        assert!(manager.key_down("f16") == DebounceResult::Activate);

        // Release F15, but F16 still held - gate should stay open
        manager.key_up("f15");

        // Release F16, now gate closes but repeat window active
        manager.key_up("f16");

        // F15 should still activate immediately (repeat window)
        assert!(manager.key_down("f15") == DebounceResult::Activate);
    }

    #[test]
    fn test_independent_groups() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.add_profile("other".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());
        manager.set_key_profile("f16".to_string(), "scroll".to_string());
        manager.set_key_profile("f17".to_string(), "other".to_string());

        // F15 activates (scroll group)
        manager.key_down("f15");
        sleep(Duration::from_millis(60));
        assert!(manager.key_down("f15") == DebounceResult::Activate);

        // F16 activates immediately (same group)
        assert!(manager.key_down("f16") == DebounceResult::Activate);

        // F17 should still need to hold (different group)
        assert!(manager.key_down("f17") == DebounceResult::Suppress);
    }

    #[test]
    fn test_no_repeat_on_held_key() {
        let mut manager = DebounceManager::new();
        manager.add_profile("scroll".to_string(), test_profile());
        manager.set_key_profile("f15".to_string(), "scroll".to_string());

        // Activate
        manager.key_down("f15");
        sleep(Duration::from_millis(60));
        assert!(manager.key_down("f15") == DebounceResult::Activate);

        // Continued holding should suppress (not spam activations)
        assert!(manager.key_down("f15") == DebounceResult::Suppress);
        assert!(manager.key_down("f15") == DebounceResult::Suppress);
        assert!(manager.key_down("f15") == DebounceResult::Suppress);
    }
}
