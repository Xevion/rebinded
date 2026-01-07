//! Key event processing strategies
//!
//! Strategies transform key events into actions with optional stateful behavior.
//! Examples include gated hold (require hold before activation), tap-vs-hold
//! detection, and double-tap recognition.
//!
//! ## Public API
//!
//! This module exposes `PlatformHandle` and `StrategyContext` as public API
//! for custom strategy implementations. Some methods may not be used internally
//! but are available for strategy authors.

mod gated_hold;

pub use gated_hold::{GatedHoldConfig, GatedHoldStrategy};

use crate::config::{Action, WindowInfo};
use crate::key::{InputEvent, InputEventId};
use crate::platform::{EventResponse, MediaCommand, Platform, PlatformInterface, SyntheticKey};
use async_trait::async_trait;
use std::collections::HashSet;
use std::time::Duration;

/// Trait for key event processing strategies.
///
/// Strategies receive key events and decide:
/// 1. Whether to block or passthrough the original key event (returned quickly)
/// 2. What actions to execute (via StrategyContext, can be immediate or delayed)
///
/// The `process` method must return quickly (< 100ms) to avoid OS hook timeouts.
/// For delayed actions, return `Block` and spawn async work via the context.
#[async_trait]
pub trait KeyStrategy: Send + Sync {
    /// Additional events this strategy wants to receive (beyond its bound keys).
    ///
    /// Strategies can subscribe to events like scroll wheel ticks that aren't
    /// directly bound to the strategy. The event handler will route these
    /// subscribed events to the strategy's `process` method.
    ///
    /// Default implementation returns an empty set (no extra subscriptions).
    fn subscriptions(&self) -> HashSet<InputEventId> {
        HashSet::new()
    }

    /// Process an input event.
    ///
    /// Must return quickly (< 100ms recommended). For delayed actions,
    /// return `EventResponse::Block` and use `ctx.execute_after()` to
    /// schedule the action.
    async fn process(&mut self, event: &InputEvent, ctx: &StrategyContext) -> EventResponse;
}

/// Wrapper to make Platform sendable across threads for delayed execution.
///
/// This uses a raw pointer internally. The Platform is guaranteed to outlive
/// all PlatformHandles in practice because the event loop owns the platform.
#[derive(Clone, Copy)]
pub struct PlatformHandle {
    ptr: *const (),
    send_media_fn: unsafe fn(*const (), MediaCommand),
    send_key_fn: unsafe fn(*const (), SyntheticKey),
    get_window_fn: unsafe fn(*const ()) -> WindowInfo,
}

// SAFETY: Platform is accessed from a single-threaded tokio runtime,
// and the pointer is valid for the lifetime of the program.
unsafe impl Send for PlatformHandle {}
unsafe impl Sync for PlatformHandle {}

impl PlatformHandle {
    /// Create a new platform handle from a reference
    ///
    /// # Safety
    /// The caller must ensure the platform outlives all uses of this handle.
    pub fn new(platform: &Platform) -> Self {
        unsafe fn send_media_impl(ptr: *const (), cmd: MediaCommand) {
            // SAFETY: Caller guarantees platform outlives all uses of this handle
            let platform = unsafe { &*(ptr as *const Platform) };
            platform.send_media(cmd);
        }
        unsafe fn send_key_impl(ptr: *const (), key: SyntheticKey) {
            // SAFETY: Caller guarantees platform outlives all uses of this handle
            let platform = unsafe { &*(ptr as *const Platform) };
            platform.send_key(key);
        }
        unsafe fn get_window_impl(ptr: *const ()) -> WindowInfo {
            // SAFETY: Caller guarantees platform outlives all uses of this handle
            let platform = unsafe { &*(ptr as *const Platform) };
            platform.get_active_window()
        }

        Self {
            ptr: platform as *const Platform as *const (),
            send_media_fn: send_media_impl,
            send_key_fn: send_key_impl,
            get_window_fn: get_window_impl,
        }
    }

    /// Create a platform handle from MockPlatform for testing
    ///
    /// # Safety
    /// The caller must ensure the MockPlatform outlives all uses of this handle.
    #[cfg(test)]
    pub unsafe fn from_mock(platform: &crate::platform::MockPlatform) -> Self {
        unsafe fn send_media_impl(ptr: *const (), cmd: MediaCommand) {
            // SAFETY: Caller guarantees MockPlatform outlives all uses of this handle
            let platform = unsafe { &*(ptr as *const crate::platform::MockPlatform) };
            platform.send_media(cmd);
        }
        unsafe fn send_key_impl(ptr: *const (), key: SyntheticKey) {
            // SAFETY: Caller guarantees MockPlatform outlives all uses of this handle
            let platform = unsafe { &*(ptr as *const crate::platform::MockPlatform) };
            platform.send_key(key);
        }
        unsafe fn get_window_impl(ptr: *const ()) -> WindowInfo {
            // SAFETY: Caller guarantees MockPlatform outlives all uses of this handle
            let platform = unsafe { &*(ptr as *const crate::platform::MockPlatform) };
            platform.get_active_window()
        }

        Self {
            ptr: platform as *const crate::platform::MockPlatform as *const (),
            send_media_fn: send_media_impl,
            send_key_fn: send_key_impl,
            get_window_fn: get_window_impl,
        }
    }

    /// Execute an action on the platform
    pub fn execute(&self, action: &Action) {
        use Action::*;
        match action {
            MediaPlayPause => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::PlayPause) },
            MediaNext => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::Next) },
            MediaPrevious => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::Previous) },
            MediaStop => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::Stop) },
            VolumeUp => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::VolumeUp) },
            VolumeDown => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::VolumeDown) },
            VolumeMute => unsafe { (self.send_media_fn)(self.ptr, MediaCommand::VolumeMute) },
            BrowserBack => unsafe { (self.send_key_fn)(self.ptr, SyntheticKey::BrowserBack) },
            BrowserForward => unsafe { (self.send_key_fn)(self.ptr, SyntheticKey::BrowserForward) },
            Passthrough | Block => {}
        }
    }

    /// Send a media command
    ///
    /// Public API method for custom strategies that need direct platform control.
    #[allow(dead_code)] // Public API for custom strategy implementations
    pub fn send_media(&self, cmd: MediaCommand) {
        unsafe { (self.send_media_fn)(self.ptr, cmd) }
    }

    /// Send a synthetic key
    ///
    /// Public API method for custom strategies that need direct platform control.
    #[allow(dead_code)] // Public API for custom strategy implementations
    pub fn send_key(&self, key: SyntheticKey) {
        unsafe { (self.send_key_fn)(self.ptr, key) }
    }

    /// Get the active window info
    pub fn get_active_window(&self) -> WindowInfo {
        unsafe { (self.get_window_fn)(self.ptr) }
    }
}

/// Context provided to strategies for action execution and platform queries.
///
/// Strategies use this to:
/// - Execute actions immediately or after a delay
/// - Query window information for conditional logic
/// - Inject synthetic keys or media commands
pub struct StrategyContext {
    platform_handle: PlatformHandle,
    action: Action,
}

impl StrategyContext {
    /// Create a new strategy context
    pub fn new(platform_handle: PlatformHandle, action: &Action) -> Self {
        Self {
            platform_handle,
            action: action.clone(),
        }
    }

    /// Execute the bound action immediately
    pub fn execute(&self) {
        self.platform_handle.execute(&self.action);
    }

    /// Execute the bound action after a delay.
    ///
    /// Spawns an async task that sleeps for `delay` then executes the action.
    /// The task runs independently — this method returns immediately.
    ///
    /// Public API method for custom strategies implementing delayed actions.
    #[allow(dead_code)] // Public API for custom strategy implementations
    pub fn execute_after(&self, delay: Duration) {
        let handle = self.platform_handle;
        let action = self.action.clone();

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            handle.execute(&action);
        });
    }

    /// Get information about the currently focused window
    ///
    /// Public API method for context-aware strategies.
    #[allow(dead_code)] // Public API for custom strategy implementations
    pub fn window_info(&self) -> WindowInfo {
        self.platform_handle.get_active_window()
    }

    /// Inject a synthetic key press
    ///
    /// Public API method for strategies that need to inject custom keys.
    #[allow(dead_code)] // Public API for custom strategy implementations
    pub fn send_key(&self, key: SyntheticKey) {
        self.platform_handle.send_key(key);
    }

    /// Send a media command
    ///
    /// Public API method for strategies that need direct media control.
    #[allow(dead_code)] // Public API for custom strategy implementations
    pub fn send_media(&self, cmd: MediaCommand) {
        self.platform_handle.send_media(cmd);
    }

    /// Get a reference to the bound action
    pub fn action(&self) -> &Action {
        &self.action
    }

    /// Get a clone of the platform handle for spawning async tasks
    pub fn platform_handle(&self) -> PlatformHandle {
        self.platform_handle
    }
}
