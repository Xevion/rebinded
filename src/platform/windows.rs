//! Windows-specific platform implementation
//!
//! Uses Win32 low-level keyboard hooks and APIs:
//! - SetWindowsHookExW(WH_KEYBOARD_LL) for intercepting keys
//! - GetForegroundWindow + GetWindowTextW for window title
//! - GetClassNameW for window class
//! - GetWindowThreadProcessId + OpenProcess + QueryFullProcessImageNameW for binary
//! - SendInput for synthetic key injection

use super::{EventResponse, MediaCommand, PlatformInterface, SyntheticKey};
use crate::config::WindowInfo;
use crate::key::{KeyCode, KeyEvent};
use anyhow::{Result, anyhow};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowTextW, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, MSG, PostThreadMessageW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::PWSTR;

/// Channel message from hook thread to main thread
struct HookEvent {
    event: KeyEvent,
    response_tx: tokio::sync::oneshot::Sender<EventResponse>,
}

/// Global state for hook callback (Win32 requires static access)
static HOOK_CHANNEL: OnceLock<mpsc::UnboundedSender<HookEvent>> = OnceLock::new();

/// Thread ID of the hook thread, used to post WM_QUIT for clean shutdown
static HOOK_THREAD_ID: OnceLock<u32> = OnceLock::new();

/// Marker for synthetic key injections so we can skip them in the hook
const INJECTED_MARKER: usize = u32::from_be_bytes(*b"RBND") as usize;

/// Windows platform implementation
pub struct Platform {
    event_rx: mpsc::UnboundedReceiver<HookEvent>,
}

// Inherent impl with public methods - this is what external code uses
impl Platform {
    /// Create a new platform instance
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Store sender in global for hook callback access
        HOOK_CHANNEL
            .set(event_tx)
            .expect("Platform::new called multiple times");

        Self { event_rx }
    }

    /// Run the platform event loop with an async handler
    ///
    /// Captures keyboard events and calls `handler` for each.
    /// The handler receives the event and a PlatformHandle for
    /// querying window info and executing actions.
    pub async fn run<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(KeyEvent, crate::strategy::PlatformHandle) -> Fut,
        Fut: std::future::Future<Output = EventResponse>,
    {
        use crate::strategy::PlatformHandle;

        info!("initializing Windows keyboard hook");

        // Spawn the hook thread (Win32 message pump must run on dedicated thread)
        let hook_handle = tokio::task::spawn_blocking(run_hook_thread);

        // Create a handle that can be passed to the handler
        let platform_handle = PlatformHandle::new(self);

        // Process events from hook thread
        while let Some(hook_event) = self.event_rx.recv().await {
            let response = handler(hook_event.event, platform_handle).await;
            // Send response back to hook thread (ignore if receiver dropped)
            let _ = hook_event.response_tx.send(response);
        }

        // Signal hook thread to exit by posting WM_QUIT
        if let Some(&thread_id) = HOOK_THREAD_ID.get() {
            info!("signaling hook thread to exit");
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }

        // Wait for hook thread to finish
        hook_handle.await??;
        Ok(())
    }

    /// Query information about the currently focused window
    pub fn get_active_window(&self) -> WindowInfo {
        get_foreground_window_info()
    }

    /// Inject a synthetic key press
    pub fn send_key(&self, key: SyntheticKey) {
        let vk = match key {
            SyntheticKey::BrowserBack => 0xA6,    // VK_BROWSER_BACK
            SyntheticKey::BrowserForward => 0xA7, // VK_BROWSER_FORWARD
        };
        send_key_press(vk);
    }

    /// Execute a media control command
    pub fn send_media(&self, cmd: MediaCommand) {
        let vk = match cmd {
            MediaCommand::PlayPause => 0xB3, // VK_MEDIA_PLAY_PAUSE
            MediaCommand::Next => 0xB0,      // VK_MEDIA_NEXT_TRACK
            MediaCommand::Previous => 0xB1,  // VK_MEDIA_PREV_TRACK
            MediaCommand::Stop => 0xB2,      // VK_MEDIA_STOP
        };
        send_key_press(vk);
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

// ============================================================================
// Hook Thread
// ============================================================================

/// Runs the Win32 message pump - must be called from a dedicated thread
fn run_hook_thread() -> Result<()> {
    unsafe {
        // Store thread ID so main thread can signal us to exit
        let thread_id = GetCurrentThreadId();
        let _ = HOOK_THREAD_ID.set(thread_id);

        // Install low-level keyboard hook
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
            .map_err(|e| anyhow!("failed to install keyboard hook: {}", e))?;

        info!("keyboard hook installed, starting message pump");

        // Message pump - required for low-level hooks to work
        // Exits when WM_QUIT is received (GetMessageW returns false)
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup (won't reach here normally)
        let _ = UnhookWindowsHookEx(hook);
        info!("keyboard hook uninstalled");
    }

    Ok(())
}

/// Low-level keyboard hook callback
/// SAFETY: Called by Windows from the message pump thread
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // code < 0 means we must pass to next hook without processing
    if code < 0 {
        // SAFETY: Windows requires us to call the next hook
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // SAFETY: lparam points to a valid KBDLLHOOKSTRUCT when code >= 0
    let kb_struct = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

    // Skip our own synthetic injections
    if kb_struct.dwExtraInfo == INJECTED_MARKER {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let vk = kb_struct.vkCode;

    let is_keydown = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let is_keyup = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);

    if !is_keydown && !is_keyup {
        // SAFETY: Windows requires us to call the next hook
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let key_code = KeyCode::new(vk);
    trace!(?key_code, is_keydown, "hook received key event");

    // Try to send event to main thread and wait for response
    let should_block = process_hook_event(key_code, is_keydown);

    if should_block {
        // Return non-zero to block the key from propagating
        LRESULT(1)
    } else {
        // SAFETY: Windows requires us to call the next hook
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

/// Send event to main thread and wait for response
fn process_hook_event(key_code: KeyCode, is_keydown: bool) -> bool {
    let Some(tx) = HOOK_CHANNEL.get() else {
        return false;
    };

    let event = KeyEvent::new(key_code, is_keydown);
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    // Send event to main thread
    if tx.send(HookEvent { event, response_tx }).is_err() {
        debug!("hook channel closed");
        return false;
    }

    // Block waiting for response (we're on hook thread, not async)
    match response_rx.blocking_recv() {
        Ok(EventResponse::Block) => true,
        Ok(EventResponse::Passthrough) => false,
        Err(_) => {
            debug!("response channel closed");
            false
        }
    }
}

// ============================================================================
// Window Queries
// ============================================================================

/// Query information about the currently focused window
fn get_foreground_window_info() -> WindowInfo {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return WindowInfo::default();
        }

        WindowInfo {
            title: get_window_title(hwnd),
            class: get_window_class(hwnd),
            binary: get_window_binary(hwnd),
        }
    }
}

/// Get the window title
unsafe fn get_window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: hwnd is a valid window handle, buffer is correctly sized
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if len > 0 {
        OsString::from_wide(&buffer[..len as usize])
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    }
}

/// Get the window class name
unsafe fn get_window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: hwnd is a valid window handle, buffer is correctly sized
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if len > 0 {
        OsString::from_wide(&buffer[..len as usize])
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    }
}

/// Get the executable name for the window's process
unsafe fn get_window_binary(hwnd: HWND) -> String {
    let mut pid = 0u32;
    // SAFETY: hwnd is a valid window handle
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return String::new();
    }

    // SAFETY: pid is a valid process ID obtained from GetWindowThreadProcessId
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return String::new();
    };

    let mut buffer = [0u16; 512];
    let mut size = buffer.len() as u32;

    // SAFETY: process is a valid handle, buffer and size are correctly initialized
    let result = if unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    }
    .is_ok()
    {
        let path = OsString::from_wide(&buffer[..size as usize])
            .to_string_lossy()
            .into_owned();

        // Extract just the filename
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    } else {
        String::new()
    };

    // SAFETY: process is a valid handle that we opened
    let _ = unsafe { CloseHandle(process) };

    result
}

// ============================================================================
// Synthetic Input
// ============================================================================

/// Send a synthetic key press (key down + key up)
///
/// Spawns a thread to avoid blocking - some keys (especially media keys)
/// can block SendInput for 600ms+ while Windows processes them.
fn send_key_press(vk: u16) {
    std::thread::spawn(move || send_key_press_sync(vk));
}

/// Synchronous implementation of key press
fn send_key_press_sync(vk: u16) {
    unsafe {
        let inputs = [
            // Key down
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(0),
                        time: 0,
                        dwExtraInfo: INJECTED_MARKER,
                    },
                },
            },
            // Key up
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: INJECTED_MARKER,
                    },
                },
            },
        ];

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != 2 {
            warn!(vk, sent, "SendInput did not send all events");
        } else {
            trace!(vk, "sent synthetic key");
        }
    }
}
