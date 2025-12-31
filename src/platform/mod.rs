//! Platform abstraction layer
//!
//! Provides a unified interface for:
//! - Input event capture (keyboard hooks)
//! - Window information queries
//! - Synthetic input (key simulation, media control)
//!
//! Each platform module (windows.rs, linux.rs) implements the same `Platform` struct
//! with inherent methods matching the `PlatformInterface` trait signature.
//! The trait exists for compile-time verification - each platform module
//! implements both inherent methods (for actual use) and the trait (for verification).

#[cfg(unix)]
mod linux;
#[cfg(windows)]
mod windows;

// Re-export the platform-specific implementation
#[cfg(unix)]
pub use linux::Platform;
#[cfg(windows)]
pub use windows::Platform;

use crate::config::WindowInfo;
use crate::key::KeyEvent;

/// Response from the event handler, telling the platform what to do with the key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResponse {
    /// Block the key from propagating to applications
    Block,
    /// Let the key pass through unchanged
    Passthrough,
}

/// Media control commands (platform-agnostic)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    Stop,
}

/// Synthetic keys that can be injected (platform-agnostic)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticKey {
    BrowserBack,
    BrowserForward,
}

use std::future::Future;

/// Interface contract for platform implementations.
///
/// Both `windows::Platform` and `linux::Platform` must implement this trait.
/// The trait is private and only used for compile-time verification that
/// both platforms have the same interface. Actual method calls go through
/// the inherent `impl Platform` methods which have the same signatures.
#[allow(dead_code, async_fn_in_trait)]
pub(crate) trait PlatformInterface {
    /// Create a new platform instance
    fn new() -> Self
    where
        Self: Sized;

    /// Run the platform event loop with an async handler
    async fn run<F, Fut>(&mut self, handler: F) -> anyhow::Result<()>
    where
        F: FnMut(KeyEvent, crate::strategy::PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>;

    /// Query information about the currently focused window
    fn get_active_window(&self) -> WindowInfo;

    /// Inject a synthetic key press
    fn send_key(&self, key: SyntheticKey);

    /// Execute a media control command
    fn send_media(&self, cmd: MediaCommand);
}
