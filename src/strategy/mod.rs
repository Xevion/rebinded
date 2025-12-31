//! Key event processing strategies
//!
//! Strategies transform key events into actions with optional stateful behavior.
//! Examples include gated hold (require hold before activation), tap-vs-hold
//! detection, and double-tap recognition.

mod gated_hold;

pub use gated_hold::{GatedHoldConfig, GatedHoldStrategy};

use crate::config::{Action, WindowInfo};
use crate::key::KeyEvent;
use crate::platform::{EventResponse, MediaCommand, Platform, SyntheticKey};
use async_trait::async_trait;
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
    /// Process a key event.
    ///
    /// Must return quickly (< 100ms recommended). For delayed actions,
    /// return `EventResponse::Block` and use `ctx.execute_after()` to
    /// schedule the action.
    async fn process(&mut self, event: &KeyEvent, ctx: &StrategyContext) -> EventResponse;
}

/// Wrapper to make Platform sendable across threads for delayed execution.
///
/// This uses a raw pointer internally. The Platform is guaranteed to outlive
/// all PlatformHandles in practice because the event loop owns the platform.
#[derive(Clone, Copy)]
pub struct PlatformHandle {
    ptr: SendPtr,
}

/// Wrapper to make raw pointer Send + Sync
#[derive(Clone, Copy)]
struct SendPtr(*const Platform);

// SAFETY: Platform is accessed from a single-threaded tokio runtime,
// and the pointer is valid for the lifetime of the program.
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

impl PlatformHandle {
    /// Create a new platform handle from a reference
    ///
    /// # Safety
    /// The caller must ensure the platform outlives all uses of this handle.
    #[allow(dead_code)] // Used by platform-specific code
    pub fn new(platform: &Platform) -> Self {
        Self {
            ptr: SendPtr(platform as *const Platform),
        }
    }

    /// Get a reference to the platform
    fn get(&self) -> &Platform {
        // SAFETY: The platform is valid for the lifetime of the program
        unsafe { &*self.ptr.0 }
    }

    /// Execute an action on the platform
    pub fn execute(&self, action: &Action) {
        action.execute(self.get());
    }

    /// Send a media command
    #[allow(dead_code)]
    pub fn send_media(&self, cmd: MediaCommand) {
        self.get().send_media(cmd);
    }

    /// Send a synthetic key
    #[allow(dead_code)]
    pub fn send_key(&self, key: SyntheticKey) {
        self.get().send_key(key);
    }

    /// Get the active window info
    pub fn get_active_window(&self) -> WindowInfo {
        self.get().get_active_window()
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
    #[allow(dead_code)]
    pub fn execute_after(&self, delay: Duration) {
        let handle = self.platform_handle;
        let action = self.action.clone();

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            handle.execute(&action);
        });
    }

    /// Get information about the currently focused window
    #[allow(dead_code)]
    pub fn window_info(&self) -> WindowInfo {
        self.platform_handle.get_active_window()
    }

    /// Inject a synthetic key press
    #[allow(dead_code)]
    pub fn send_key(&self, key: SyntheticKey) {
        self.platform_handle.send_key(key);
    }

    /// Send a media command
    #[allow(dead_code)]
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
