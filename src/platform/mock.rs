//! Mock platform implementation for testing
//!
//! This mock platform records all platform calls instead of executing them,
//! preventing tests from triggering real media controls, key presses, or
//! other system-level side effects.
//!
//! TODO: Consider exposing this as a "dry-run" mode via CLI flag for users
//! to test their configuration without executing actions.

use super::{EventResponse, MediaCommand, PlatformInterface, SyntheticKey};
use crate::config::WindowInfo;
use crate::key::InputEvent;
use crate::strategy::PlatformHandle;
use anyhow::Result;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Recorded platform call
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformCall {
    SendMedia(MediaCommand),
    SendKey(SyntheticKey),
}

/// Mock platform that records calls instead of executing them
#[derive(Clone)]
pub struct MockPlatform {
    calls: Arc<Mutex<Vec<PlatformCall>>>,
}

impl MockPlatform {
    /// Get all recorded calls
    pub fn calls(&self) -> Vec<PlatformCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Clear all recorded calls
    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }

    /// Assert that a specific media command was sent
    pub fn assert_media_sent(&self, cmd: MediaCommand) {
        let calls = self.calls();
        assert!(
            calls.contains(&PlatformCall::SendMedia(cmd)),
            "Expected SendMedia({:?}) but got calls: {:?}",
            cmd,
            calls
        );
    }

    /// Assert that no calls were made
    pub fn assert_no_calls(&self) {
        let calls = self.calls();
        assert!(calls.is_empty(), "Expected no calls but got: {:?}", calls);
    }

    /// Assert that exactly N calls were made
    pub fn assert_call_count(&self, expected: usize) {
        let calls = self.calls();
        assert_eq!(
            calls.len(),
            expected,
            "Expected {} calls but got {}: {:?}",
            expected,
            calls.len(),
            calls
        );
    }
}

impl MockPlatform {
    /// Create a new mock platform
    pub fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl PlatformInterface for MockPlatform {
    fn new() -> Self {
        MockPlatform::new()
    }

    async fn run<F, Fut>(&mut self, _handler: F) -> Result<()>
    where
        F: FnMut(InputEvent, PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>,
    {
        // No-op for tests - tests don't call run()
        Ok(())
    }

    fn get_active_window(&self) -> WindowInfo {
        // Return default/empty window info for tests
        WindowInfo::default()
    }

    fn send_key(&self, key: SyntheticKey) {
        // Record instead of executing
        self.calls.lock().unwrap().push(PlatformCall::SendKey(key));
    }

    fn send_media(&self, cmd: MediaCommand) {
        // Record instead of executing
        self.calls
            .lock()
            .unwrap()
            .push(PlatformCall::SendMedia(cmd));
    }
}
