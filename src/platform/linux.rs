//! Linux-specific platform implementation
//!
//! Key components (TODO):
//! - evdev for raw input device access (requires /dev/input permissions)
//! - X11 (via x11rb) or Wayland for window queries
//! - uinput for synthetic input injection
//! - D-Bus MPRIS for media control

use super::{EventResponse, MediaCommand, PlatformInterface, SyntheticKey};
use crate::config::WindowInfo;
use crate::key::KeyEvent;
use anyhow::Result;
use std::time::Duration;
use tracing::{info, warn};

/// Linux platform implementation
pub struct Platform {}

// Inherent impl with public methods - this is what external code uses
impl Platform {
    /// Create a new platform instance
    pub fn new() -> Self {
        Self {}
    }

    /// Run the platform event loop with an async handler
    pub async fn run<F, Fut>(&mut self, mut _handler: F) -> Result<()>
    where
        F: FnMut(KeyEvent, crate::strategy::PlatformHandle) -> Fut,
        Fut: std::future::Future<Output = EventResponse>,
    {
        info!("starting Linux input handler");

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

    /// Query information about the currently focused window
    pub fn get_active_window(&self) -> WindowInfo {
        // TODO: Implement using x11rb
        // 1. Get root window
        // 2. Get _NET_ACTIVE_WINDOW property -> active window ID
        // 3. Get _NET_WM_NAME or WM_NAME property -> title
        // 4. Get WM_CLASS property -> (instance, class)
        // 5. Get _NET_WM_PID -> pid -> read /proc/<pid>/exe -> binary name
        WindowInfo::default()
    }

    /// Inject a synthetic key press
    pub fn send_key(&self, key: SyntheticKey) {
        // TODO: Implement using uinput or xdotool
        // For browser back/forward, could also send Alt+Left / Alt+Right
        warn!(?key, "send_key not implemented on Linux");
    }

    /// Execute a media control command
    pub fn send_media(&self, cmd: MediaCommand) {
        // TODO: Implement using D-Bus MPRIS (more reliable than key simulation)
        // Could use zbus crate for D-Bus, or shell out to playerctl
        warn!(?cmd, "send_media not implemented on Linux");
    }
}

impl Default for Platform {
    fn default() -> Self {
        Self::new()
    }
}

// Trait impl for compile-time interface verification only
impl PlatformInterface for Platform {
    fn new() -> Self {
        Self::new()
    }

    async fn run<F, Fut>(&mut self, handler: F) -> Result<()>
    where
        F: FnMut(KeyEvent, crate::strategy::PlatformHandle) -> Fut,
        Fut: std::future::Future<Output = EventResponse>,
    {
        Self::run(self, handler).await
    }

    fn get_active_window(&self) -> WindowInfo {
        Self::get_active_window(self)
    }

    fn send_key(&self, key: SyntheticKey) {
        Self::send_key(self, key)
    }

    fn send_media(&self, cmd: MediaCommand) {
        Self::send_media(self, cmd)
    }
}
