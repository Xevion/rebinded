//! Platform-agnostic key code abstraction
//!
//! This module provides a universal key representation that works across platforms
//! without hardcoding key lists. Key codes are platform-native (VK codes on Windows,
//! evdev codes on Linux), and display names are queried from the OS on demand.

use std::collections::HashMap;
use std::sync::OnceLock;

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
// Platform-specific implementations
// ============================================================================

#[cfg(windows)]
mod platform_impl {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyNameTextW, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
    };

    /// Get human-readable key name from Windows VK code
    pub fn get_key_name(vk: u32) -> String {
        unsafe {
            let mut buffer = [0u16; 64];

            let scan_code = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX);
            let extended = (scan_code & 0xFF00) == 0xE000 || (scan_code & 0xFF00) == 0xE100;
            let lparam = ((scan_code & 0xFF) << 16) | (u32::from(extended) << 24);

            let len = GetKeyNameTextW(lparam as i32, &mut buffer);
            if len > 0 {
                OsString::from_wide(&buffer[..len as usize])
                    .to_string_lossy()
                    .into_owned()
            } else {
                // Fallback for keys without names
                format!("VK_{:#04X}", vk)
            }
        }
    }

    /// Build reverse lookup map: name -> VK code
    pub fn build_name_map() -> HashMap<String, u32> {
        let mut map = HashMap::new();

        // Hardcoded keys that GetKeyNameTextW doesn't provide names for.
        // These have no scan codes on standard keyboards, so Windows can't
        // look them up. We add these first so OS-provided names can override
        // if available (they won't be, but it's defensive).
        #[rustfmt::skip]
        const HARDCODED_KEYS: &[(&str, u32)] = &[
            // F13-F24: no physical keys on standard keyboards
            ("f13", 0x7C), ("f14", 0x7D), ("f15", 0x7E), ("f16", 0x7F),
            ("f17", 0x80), ("f18", 0x81), ("f19", 0x82), ("f20", 0x83),
            ("f21", 0x84), ("f22", 0x85), ("f23", 0x86), ("f24", 0x87),

            // Media keys
            ("media_next_track", 0xB0), ("media_next", 0xB0),
            ("media_prev_track", 0xB1), ("media_prev", 0xB1),
            ("media_stop", 0xB2),
            ("media_play_pause", 0xB3), ("media_play", 0xB3),
            ("volume_mute", 0xAD), ("mute", 0xAD),
            ("volume_down", 0xAE), ("vol_down", 0xAE),
            ("volume_up", 0xAF), ("vol_up", 0xAF),

            // Browser keys
            ("browser_back", 0xA6), ("browser_forward", 0xA7),
            ("browser_refresh", 0xA8), ("browser_stop", 0xA9),
            ("browser_search", 0xAA), ("browser_favorites", 0xAB),
            ("browser_home", 0xAC),

            // Launch keys
            ("launch_mail", 0xB4), ("launch_media_select", 0xB5),
            ("launch_app1", 0xB6), ("launch_app2", 0xB7),

            // Left/right modifier variants (GetKeyNameTextW returns generic names)
            ("lshift", 0xA0), ("rshift", 0xA1),
            ("lctrl", 0xA2), ("lcontrol", 0xA2),
            ("rctrl", 0xA3), ("rcontrol", 0xA3),
            ("lalt", 0xA4), ("lmenu", 0xA4),
            ("ralt", 0xA5), ("rmenu", 0xA5),
            ("lwin", 0x5B), ("rwin", 0x5C),

            // Common aliases for consistency
            ("space", 0x20), ("spacebar", 0x20),
            ("enter", 0x0D), ("return", 0x0D),
            ("esc", 0x1B), ("escape", 0x1B),
            ("backspace", 0x08), ("back", 0x08),
            ("tab", 0x09),
            ("insert", 0x2D), ("ins", 0x2D),
            ("delete", 0x2E), ("del", 0x2E),
            ("home", 0x24), ("end", 0x23),
            ("pageup", 0x21), ("page_up", 0x21), ("pgup", 0x21),
            ("pagedown", 0x22), ("page_down", 0x22), ("pgdn", 0x22),
            ("up", 0x26), ("down", 0x28), ("left", 0x25), ("right", 0x27),
            ("capslock", 0x14), ("caps_lock", 0x14), ("caps", 0x14),
            ("numlock", 0x90), ("num_lock", 0x90),
            ("scrolllock", 0x91), ("scroll_lock", 0x91),
            ("printscreen", 0x2C), ("print_screen", 0x2C), ("prtsc", 0x2C),
            ("pause", 0x13),

            // Numpad keys
            ("numpad0", 0x60), ("numpad1", 0x61), ("numpad2", 0x62),
            ("numpad3", 0x63), ("numpad4", 0x64), ("numpad5", 0x65),
            ("numpad6", 0x66), ("numpad7", 0x67), ("numpad8", 0x68),
            ("numpad9", 0x69),
            ("numpad_add", 0x6B), ("numpad_plus", 0x6B),
            ("numpad_subtract", 0x6D), ("numpad_minus", 0x6D),
            ("numpad_multiply", 0x6A), ("numpad_mul", 0x6A),
            ("numpad_divide", 0x6F), ("numpad_div", 0x6F),
            ("numpad_decimal", 0x6E), ("numpad_dot", 0x6E),
        ];

        for &(name, vk) in HARDCODED_KEYS {
            map.insert(name.to_string(), vk);
        }

        // Probe all VK codes for OS-provided names (may override hardcoded)
        for vk in 0..=255 {
            let name = get_key_name(vk);
            if !name.is_empty() && !name.starts_with("VK_") {
                // Normalize: lowercase, store both raw and prefixed versions
                let normalized = name.to_lowercase();
                map.insert(normalized.clone(), vk);

                // Also store with "vk_" prefix for compatibility
                map.insert(format!("vk_{}", normalized), vk);
            }
        }

        map
    }
}

#[cfg(unix)]
mod platform_impl {
    use super::*;

    /// Get human-readable key name from Linux evdev code
    pub fn get_key_name(code: u32) -> String {
        // evdev codes are u16
        if code > u16::MAX as u32 {
            return format!("UNKNOWN_{:#06X}", code);
        }

        // Use evdev's Debug impl which gives us "KEY_F13" etc.
        format!("{:?}", evdev::Key::new(code as u16))
    }

    /// Build reverse lookup map: name -> evdev code
    pub fn build_name_map() -> HashMap<String, u32> {
        let mut map = HashMap::new();

        // Probe evdev key range (0-767 covers all standard keys)
        for code in 0..768u32 {
            let name = get_key_name(code);
            if !name.starts_with("UNKNOWN") {
                // Normalize: lowercase
                let normalized = name.to_lowercase();
                map.insert(normalized.clone(), code);

                // Strip "KEY_" prefix for convenience: "KEY_F13" -> "f13"
                if let Some(short) = normalized.strip_prefix("key_") {
                    map.insert(short.to_string(), code);
                }
                // Strip "BTN_" prefix for buttons
                if let Some(short) = normalized.strip_prefix("btn_") {
                    map.insert(short.to_string(), code);
                }
            }
        }

        map
    }
}

// Platform-agnostic interface
use platform_impl::{build_name_map, get_key_name};

fn platform_key_name(code: u32) -> String {
    get_key_name(code)
}

/// Lazy-initialized reverse lookup map
static NAME_TO_CODE: OnceLock<HashMap<String, u32>> = OnceLock::new();

fn platform_key_from_name(name: &str) -> Option<KeyCode> {
    let map = NAME_TO_CODE.get_or_init(build_name_map);
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
