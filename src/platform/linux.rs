//! Linux-specific implementation
//!
//! Key components:
//! - evdev for raw input device access (requires /dev/input permissions)
//! - X11 (via x11rb) or Wayland for window queries
//! - uinput for synthetic input injection
//!
//! Alternative approaches:
//! - keyd (system-level, might be simpler for basic remapping)
//! - libinput for input handling

use crate::config::{Config, WindowInfo};
use anyhow::Result;
use tracing::{info, warn};

pub async fn run(config: Config) -> Result<()> {
    info!("starting Linux input handler");

    // TODO: Implement Linux input handling
    // Options:
    // 1. evdev: Read from /dev/input/eventX, grab the device, filter F13-F24
    // 2. Use libinput for higher-level input handling
    //
    // For window queries:
    // - X11: Use x11rb crate, query _NET_ACTIVE_WINDOW, then WM_NAME, WM_CLASS
    // - Wayland: More complex, compositor-specific (wlr-foreign-toplevel-management)

    warn!("Linux handler not yet implemented - running placeholder loop");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// Query information about the currently focused window (X11)
fn get_active_window_info_x11() -> WindowInfo {
    // TODO: Implement using x11rb
    // 1. Get root window
    // 2. Get _NET_ACTIVE_WINDOW property -> active window ID
    // 3. Get _NET_WM_NAME or WM_NAME property -> title
    // 4. Get WM_CLASS property -> (instance, class)
    // 5. Get _NET_WM_PID -> pid -> read /proc/<pid>/exe -> binary name

    WindowInfo::default()
}

/// Convert evdev key code to our key name
fn evdev_to_name(code: u16) -> Option<&'static str> {
    // evdev KEY_F13 through KEY_F24 codes
    // These are defined in linux/input-event-codes.h
    match code {
        183 => Some("f13"), // KEY_F13
        184 => Some("f14"),
        185 => Some("f15"),
        186 => Some("f16"),
        187 => Some("f17"),
        188 => Some("f18"),
        189 => Some("f19"),
        190 => Some("f20"),
        191 => Some("f21"),
        192 => Some("f22"),
        193 => Some("f23"),
        194 => Some("f24"),
        _ => None,
    }
}
