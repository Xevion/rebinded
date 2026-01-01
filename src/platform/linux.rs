//! Linux-specific platform implementation
//!
//! Key components (TODO):
//! - evdev for raw input device access (requires /dev/input permissions)
//! - X11 (via x11rb) or Wayland for window queries
//! - uinput for synthetic input injection
//! - D-Bus MPRIS for media control

use super::{EventResponse, MediaCommand, PlatformInterface, SyntheticKey};
use crate::config::WindowInfo;
use crate::key::InputEvent;
use crate::strategy::PlatformHandle;
use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use tracing::{info, warn};

// ============================================================================
// Key Name Resolution
// ============================================================================

/// Get human-readable key name from Linux evdev code
pub fn get_key_name(code: u32) -> String {
    if code > u16::MAX as u32 {
        return format!("UNKNOWN_{:#06X}", code);
    }
    format!("{:?}", evdev::Key::new(code as u16))
}

/// Build reverse lookup map: name -> evdev code
pub fn build_key_name_map() -> HashMap<String, u32> {
    let mut map = HashMap::new();

    // Probe evdev key range (0-767 covers all standard keys)
    for code in 0..768u32 {
        let name = get_key_name(code);
        if !name.starts_with("UNKNOWN") {
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

/// Linux platform implementation
pub struct Platform {}

impl Default for Platform {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformInterface for Platform {
    fn new() -> Self {
        Self {}
    }

    async fn run<F, Fut>(&mut self, mut _handler: F) -> Result<()>
    where
        F: FnMut(InputEvent, PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>,
    {
        info!("starting Linux input handler");

        // TODO: Implement scroll event capture via evdev REL_WHEEL

        // TODO: Implement Linux input handling
        // Options:
        // 1. evdev: Read from /dev/input/eventX, grab the device, filter F13-F24
        // 2. Use libinput for higher-level input handling
        //
        // For window queries:
        // - X11: Use x11rb crate, query _NET_ACTIVE_WINDOW, then WM_NAME, WM_CLASS
        // - Wayland: More complex, compositor-specific (wlr-foreign-toplevel-management)

        warn!("Linux platform not yet implemented - running placeholder loop");

        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    fn get_active_window(&self) -> WindowInfo {
        // TODO: Implement using x11rb
        // 1. Get root window
        // 2. Get _NET_ACTIVE_WINDOW property -> active window ID
        // 3. Get _NET_WM_NAME or WM_NAME property -> title
        // 4. Get WM_CLASS property -> (instance, class)
        // 5. Get _NET_WM_PID -> pid -> read /proc/<pid>/exe -> binary name
        WindowInfo::default()
    }

    fn send_key(&self, key: SyntheticKey) {
        // TODO: Implement using uinput or xdotool
        // For browser back/forward, could also send Alt+Left / Alt+Right
        warn!(?key, "send_key not implemented on Linux");
    }

    fn send_media(&self, cmd: MediaCommand) {
        // TODO: Implement using D-Bus MPRIS (more reliable than key simulation)
        // Could use zbus crate for D-Bus, or shell out to playerctl
        warn!(?cmd, "send_media not implemented on Linux");
    }
}
