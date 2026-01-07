//! Platform abstraction layer
//!
//! Provides a unified interface for:
//! - Input event capture (keyboard hooks)
//! - Window information queries
//! - Synthetic input (key simulation, media control)
//! - Key name resolution (OS-specific key code <-> name mapping)
//!
//! Each platform module (windows.rs, linux.rs) exports a `Platform` struct that
//! implements the `PlatformInterface` trait. The trait is the primary interface -
//! all platform methods are called through it. Since only one platform is compiled
//! per target (via cfg), the compiler monomorphizes all trait calls, giving us
//! zero-cost abstraction with compile-time contract verification.

#[cfg(unix)]
mod linux;
#[cfg(windows)]
mod windows;

// Re-export the platform-specific implementation
#[cfg(unix)]
pub use linux::{Platform, build_key_name_map, get_key_name};
#[cfg(windows)]
pub use windows::{Platform, build_key_name_map, get_key_name};

use std::future::Future;

use crate::config::WindowInfo;
use crate::key::InputEvent;

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
    VolumeUp,
    VolumeDown,
    VolumeMute,
}

/// Synthetic keys that can be injected (platform-agnostic)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticKey {
    BrowserBack,
    BrowserForward,
}

/// Interface contract for platform implementations.
///
/// Both `windows::Platform` and `linux::Platform` implement this trait.
/// All platform methods are called through this trait. Since only one
/// platform is compiled per target (via cfg), the compiler monomorphizes
/// all calls - no vtable overhead.
#[allow(async_fn_in_trait)]
pub trait PlatformInterface {
    /// Create a new platform instance
    fn new() -> Self
    where
        Self: Sized;

    /// Run the platform event loop with an async handler
    async fn run<F, Fut>(&mut self, handler: F) -> anyhow::Result<()>
    where
        F: FnMut(InputEvent, crate::strategy::PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>;

    /// Query information about the currently focused window
    fn get_active_window(&self) -> WindowInfo;

    /// Inject a synthetic key press
    fn send_key(&self, key: SyntheticKey);

    /// Execute a media control command
    fn send_media(&self, cmd: MediaCommand);
}

// Mock platform for testing
#[cfg(test)]
pub(crate) mod mock;

#[cfg(test)]
pub(crate) use mock::MockPlatform;
