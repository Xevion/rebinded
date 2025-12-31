//! Windows-specific implementation using Win32 low-level keyboard hooks
//!
//! Key components:
//! - SetWindowsHookExW with WH_KEYBOARD_LL for intercepting keys
//! - GetForegroundWindow + GetWindowTextW for window title
//! - GetWindowThreadProcessId + QueryFullProcessImageNameW for binary name
//! - GetClassNameW for window class

use crate::config::{Config, WindowInfo};
use anyhow::Result;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

// Global state for the hook callback (Win32 hooks require static/global access)
static CONFIG: OnceLock<Config> = OnceLock::new();

pub async fn run(config: Config) -> Result<()> {
    CONFIG.set(config).expect("config already initialized");

    info!("starting Windows keyboard hook");

    // TODO: Implement the actual hook
    // 1. SetWindowsHookExW(WH_KEYBOARD_LL, callback, ...)
    // 2. Message pump loop (GetMessage/TranslateMessage/DispatchMessage)
    // 3. In callback: check if key is F13-F24, if so:
    //    - Get current window info
    //    - Resolve action from config
    //    - Execute action or passthrough

    warn!("Windows hook not yet implemented - running placeholder loop");

    // Placeholder: just keep the daemon alive
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// Query information about the currently focused window
fn get_foreground_window_info() -> WindowInfo {
    // TODO: Implement using Win32 APIs
    // - GetForegroundWindow() -> HWND
    // - GetWindowTextW(hwnd, ...) -> title
    // - GetClassNameW(hwnd, ...) -> class
    // - GetWindowThreadProcessId(hwnd, ...) -> pid
    // - OpenProcess(pid) + QueryFullProcessImageNameW -> binary path -> extract filename

    WindowInfo::default()
}

/// Convert a Windows virtual key code to our key name
fn vk_to_name(vk: u32) -> Option<&'static str> {
    // F13-F24 virtual key codes are 0x7C-0x87
    match vk {
        0x7C => Some("f13"),
        0x7D => Some("f14"),
        0x7E => Some("f15"),
        0x7F => Some("f16"),
        0x80 => Some("f17"),
        0x81 => Some("f18"),
        0x82 => Some("f19"),
        0x83 => Some("f20"),
        0x84 => Some("f21"),
        0x85 => Some("f22"),
        0x86 => Some("f23"),
        0x87 => Some("f24"),
        _ => None,
    }
}
