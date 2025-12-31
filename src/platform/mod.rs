//! Platform-specific input handling and window queries
//!
//! Each platform must implement:
//! - Low-level keyboard hook to intercept F13-F24
//! - Window info queries (title, class, binary)
//! - Key event simulation for passthrough/remapping

use crate::config::Config;
use anyhow::Result;

#[cfg(windows)]
mod windows;

#[cfg(unix)]
mod linux;

/// Run the platform-specific event loop
pub async fn run(config: Config) -> Result<()> {
    #[cfg(windows)]
    {
        windows::run(config).await
    }

    #[cfg(unix)]
    {
        linux::run(config).await
    }
}

/// Information about a key event
#[derive(Debug, Clone)]
pub struct KeyEvent {
    /// Virtual key code (platform-specific)
    pub vk: u32,
    /// Key name (e.g., "f13", "f14")
    pub name: String,
    /// Whether this is a key-down (true) or key-up (false) event
    pub down: bool,
}
