//! Platform-agnostic key code abstraction
//!
//! This module provides a universal key representation that works across platforms
//! without hardcoding key lists. Key codes are platform-native (VK codes on Windows,
//! evdev codes on Linux), and display names are queried from the platform layer.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::platform;

/// Platform-agnostic key code.
///
/// Stores the raw OS-specific key code internally. Display names are queried
/// from the OS on demand via `display_name()`, not stored in the struct.
///
/// # Examples
///
/// ```ignore
/// // From platform-native code
/// let key = KeyCode::new(0x7C); // VK_F13 on Windows
///
/// // Display name
/// println!("Key pressed: {}", key.display_name()); // "F13"
///
/// // Parse from config string
/// let key = KeyCode::from_config_str("f13").unwrap();
/// let key = KeyCode::from_config_str("0x7C").unwrap();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCode(u32);

impl KeyCode {
    /// Create a KeyCode from a raw platform-native code
    #[allow(dead_code)] // Used by platform-specific code
    pub fn new(code: u32) -> Self {
        Self(code)
    }

    /// Get human-readable display name from the OS
    ///
    /// Returns OS-provided names like "F13", "Space", "Enter" on Windows,
    /// or "KEY_F13", "KEY_SPACE" on Linux.
    pub fn display_name(&self) -> String {
        platform_key_name(self.0)
    }

    /// Parse a key specifier from config
    ///
    /// Accepts:
    /// - Hex literals: "0x7C", "0X7c"
    /// - Decimal numbers: "124"
    /// - Key names: "f13", "KEY_F13", "space"
    ///
    /// Numbers are treated as raw codes. Names are looked up via the OS.
    pub fn from_config_str(s: &str) -> Option<Self> {
        parse_key_specifier(s)
    }
}

impl std::fmt::Display for KeyCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// A key event received from the platform
#[derive(Debug, Clone)]
pub struct KeyEvent {
    /// The key that was pressed/released
    pub key: KeyCode,
    /// Whether this is a key-down (true) or key-up (false) event
    pub down: bool,
}

impl KeyEvent {
    /// Create a new key event
    #[allow(dead_code)] // Used by platform-specific code
    pub fn new(key: KeyCode, down: bool) -> Self {
        Self { key, down }
    }
}

/// Parse a key specifier from config
///
/// Tries in order: hex literal, decimal number, key name lookup
fn parse_key_specifier(s: &str) -> Option<KeyCode> {
    // Try hex: "0x7C" -> 124
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
        && let Ok(code) = u32::from_str_radix(hex, 16)
    {
        return Some(KeyCode(code));
    }

    // Try decimal if all digits: "124" -> 124
    if s.chars().all(|c| c.is_ascii_digit())
        && let Ok(code) = s.parse::<u32>()
    {
        return Some(KeyCode(code));
    }

    // Otherwise treat as name: "f13", "KEY_F13", etc.
    platform_key_from_name(s)
}

// ============================================================================
// Platform-agnostic wrappers
// ============================================================================

fn platform_key_name(code: u32) -> String {
    platform::get_key_name(code)
}

/// Lazy-initialized reverse lookup map
static NAME_TO_CODE: OnceLock<HashMap<String, u32>> = OnceLock::new();

fn platform_key_from_name(name: &str) -> Option<KeyCode> {
    let map = NAME_TO_CODE.get_or_init(platform::build_key_name_map);
    let normalized = name.to_lowercase();
    map.get(&normalized).copied().map(KeyCode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex() {
        let key = parse_key_specifier("0x7C").unwrap();
        assert!(key.0 == 124);

        let key = parse_key_specifier("0X7c").unwrap();
        assert!(key.0 == 124);
    }

    #[test]
    fn test_parse_decimal() {
        let key = parse_key_specifier("124").unwrap();
        assert!(key.0 == 124);

        let key = parse_key_specifier("48").unwrap();
        assert!(key.0 == 48);
    }

    #[test]
    fn test_parse_name() {
        // This test will vary by platform, so just verify it doesn't panic
        let _ = parse_key_specifier("f13");
        let _ = parse_key_specifier("KEY_F13");
    }

    #[test]
    fn test_display_name() {
        let key = KeyCode::new(124);
        let name = key.display_name();
        assert!(!name.is_empty());
    }
}
