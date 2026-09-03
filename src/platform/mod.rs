//! Platform abstraction layer
//!
//! Provides a unified interface for input capture, window queries, synthetic
//! input, and key name resolution. Each platform module exports a `Platform`
//! implementing `PlatformInterface`.

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
/// Only one platform is compiled per target, so calls monomorphize with no
/// vtable overhead.
#[allow(async_fn_in_trait)]
pub trait PlatformInterface {
    /// Create a new platform instance
    fn new() -> Self
    where
        Self: Sized;

    /// Run the platform event loop with an async handler.
    ///
    /// `bound_keys` is every key the config can act on; platforms that take
    /// devices exclusively claim only those that can produce one.
    async fn run<F, Fut>(
        &mut self,
        bound_keys: &std::collections::HashSet<crate::key::KeyCode>,
        handler: F,
    ) -> anyhow::Result<()>
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
