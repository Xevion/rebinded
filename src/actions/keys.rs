//! Key simulation actions
//!
//! Simulates keypresses for browser navigation and other key-based actions.
//!
//! Platform implementations:
//! - Windows: SendInput with KEYBDINPUT
//! - Linux: uinput or XTest

use anyhow::Result;
use tracing::warn;

pub async fn browser_back() -> Result<()> {
    #[cfg(windows)]
    {
        // VK_BROWSER_BACK = 0xA6
        // Or simulate Alt+Left
        warn!("browser back not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // XF86Back key, or Alt+Left
        // Could use xdotool: xdotool key alt+Left
        warn!("browser back not implemented on Linux");
    }

    Ok(())
}

pub async fn browser_forward() -> Result<()> {
    #[cfg(windows)]
    {
        // VK_BROWSER_FORWARD = 0xA7
        // Or simulate Alt+Right
        warn!("browser forward not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // XF86Forward key, or Alt+Right
        warn!("browser forward not implemented on Linux");
    }

    Ok(())
}

/// Send a raw virtual key press (down + up)
#[allow(dead_code)]
pub async fn send_key(vk: u32) -> Result<()> {
    #[cfg(windows)]
    {
        // TODO: SendInput with KEYBDINPUT { wVk: vk, ... }
        warn!(vk, "send_key not implemented on Windows");
    }

    #[cfg(unix)]
    {
        // TODO: uinput or XTest
        warn!(vk, "send_key not implemented on Linux");
    }

    Ok(())
}
