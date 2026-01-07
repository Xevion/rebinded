//! Windows-specific platform implementation
//!
//! Uses Win32 low-level keyboard hooks and APIs:
//! - SetWindowsHookExW(WH_KEYBOARD_LL) for intercepting keys
//! - GetForegroundWindow + GetWindowTextW for window title
//! - GetClassNameW for window class
//! - GetWindowThreadProcessId + OpenProcess + QueryFullProcessImageNameW for binary
//! - SendInput for synthetic key injection
//! - GetKeyNameTextW + MapVirtualKeyW for key name resolution

use super::{EventResponse, MediaCommand, PlatformInterface, SyntheticKey};
use crate::config::WindowInfo;
use crate::key::{InputEvent, KeyCode, KeyEvent};
use crate::strategy::PlatformHandle;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::os::windows::ffi::OsStringExt;
use std::sync::OnceLock;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowTextW, GetWindowThreadProcessId, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT,
    PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MOUSEWHEEL, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::PWSTR;

// ============================================================================
// Key Name Resolution
// ============================================================================

/// Keys without scan codes on standard keyboards (GetKeyNameTextW can't look them up)
#[rustfmt::skip]
const HARDCODED_KEYS: &[(&str, u32)] = &[
    // F13-F24
    ("f13", 0x7C), ("f14", 0x7D), ("f15", 0x7E), ("f16", 0x7F),
    ("f17", 0x80), ("f18", 0x81), ("f19", 0x82), ("f20", 0x83),
    ("f21", 0x84), ("f22", 0x85), ("f23", 0x86), ("f24", 0x87),

    // Media keys
    ("media_next_track", 0xB0), ("media_next", 0xB0),
    ("media_prev_track", 0xB1), ("media_prev", 0xB1),
    ("media_stop", 0xB2),
    ("media_play_pause", 0xB3), ("media_play", 0xB3),
    ("volume_mute", 0xAD), ("mute", 0xAD),
    ("volume_down", 0xAE), ("vol_down", 0xAE),
    ("volume_up", 0xAF), ("vol_up", 0xAF),

    // Browser keys
    ("browser_back", 0xA6), ("browser_forward", 0xA7),
    ("browser_refresh", 0xA8), ("browser_stop", 0xA9),
    ("browser_search", 0xAA), ("browser_favorites", 0xAB),
    ("browser_home", 0xAC),

    // Launch keys
    ("launch_mail", 0xB4), ("launch_media_select", 0xB5),
    ("launch_app1", 0xB6), ("launch_app2", 0xB7),

    // Left/right modifier variants (GetKeyNameTextW returns generic names)
    ("lshift", 0xA0), ("rshift", 0xA1),
    ("lctrl", 0xA2), ("lcontrol", 0xA2),
    ("rctrl", 0xA3), ("rcontrol", 0xA3),
    ("lalt", 0xA4), ("lmenu", 0xA4),
    ("ralt", 0xA5), ("rmenu", 0xA5),
    ("lwin", 0x5B), ("rwin", 0x5C),

    // Common aliases
    ("space", 0x20), ("spacebar", 0x20),
    ("enter", 0x0D), ("return", 0x0D),
    ("esc", 0x1B), ("escape", 0x1B),
    ("backspace", 0x08), ("back", 0x08),
    ("tab", 0x09),
    ("insert", 0x2D), ("ins", 0x2D),
    ("delete", 0x2E), ("del", 0x2E),
    ("home", 0x24), ("end", 0x23),
    ("pageup", 0x21), ("page_up", 0x21), ("pgup", 0x21),
    ("pagedown", 0x22), ("page_down", 0x22), ("pgdn", 0x22),
    ("up", 0x26), ("down", 0x28), ("left", 0x25), ("right", 0x27),
    ("capslock", 0x14), ("caps_lock", 0x14), ("caps", 0x14),
    ("numlock", 0x90), ("num_lock", 0x90),
    ("scrolllock", 0x91), ("scroll_lock", 0x91),
    ("printscreen", 0x2C), ("print_screen", 0x2C), ("prtsc", 0x2C),
    ("pause", 0x13),

    // Numpad keys
    ("numpad0", 0x60), ("numpad1", 0x61), ("numpad2", 0x62),
    ("numpad3", 0x63), ("numpad4", 0x64), ("numpad5", 0x65),
    ("numpad6", 0x66), ("numpad7", 0x67), ("numpad8", 0x68),
    ("numpad9", 0x69),
    ("numpad_add", 0x6B), ("numpad_plus", 0x6B),
    ("numpad_subtract", 0x6D), ("numpad_minus", 0x6D),
    ("numpad_multiply", 0x6A), ("numpad_mul", 0x6A),
    ("numpad_divide", 0x6F), ("numpad_div", 0x6F),
    ("numpad_decimal", 0x6E), ("numpad_dot", 0x6E),
];

/// Get human-readable key name from Windows VK code
pub fn get_key_name(vk: u32) -> String {
    unsafe {
        let mut buffer = [0u16; 64];

        let scan_code = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC_EX);
        let extended = (scan_code & 0xFF00) == 0xE000 || (scan_code & 0xFF00) == 0xE100;
        let lparam = ((scan_code & 0xFF) << 16) | (u32::from(extended) << 24);

        let len = GetKeyNameTextW(lparam as i32, &mut buffer);
        if len > 0 {
            OsString::from_wide(&buffer[..len as usize])
                .to_string_lossy()
                .into_owned()
        } else {
            format!("VK_{:#04X}", vk)
        }
    }
}

/// Build reverse lookup map: name -> VK code
pub fn build_key_name_map() -> HashMap<String, u32> {
    let mut map = HashMap::new();

    // Add hardcoded keys first (OS-provided names can override if available)
    for &(name, vk) in HARDCODED_KEYS {
        map.insert(name.to_string(), vk);
    }

    // Probe all VK codes for OS-provided names
    for vk in 0..=255 {
        let name = get_key_name(vk);
        if !name.is_empty() && !name.starts_with("VK_") {
            let normalized = name.to_lowercase();
            map.insert(normalized.clone(), vk);
            map.insert(format!("vk_{}", normalized), vk);
        }
    }

    map
}

// ============================================================================
// Hook Thread
// ============================================================================

/// Channel message from hook thread to main thread
struct HookEvent {
    event: InputEvent,
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

impl Default for Platform {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformInterface for Platform {
    fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Store sender in global for hook callback access
        HOOK_CHANNEL
            .set(event_tx)
            .expect("Platform::new called multiple times");

        Self { event_rx }
    }

    /// Run the platform event loop with an async handler
    ///
    /// Captures keyboard and mouse wheel events and calls `handler` for each.
    /// The handler receives the event and a PlatformHandle for
    /// querying window info and executing actions.
    async fn run<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(InputEvent, PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>,
    {
        info!("initializing Windows input hooks");

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

    fn get_active_window(&self) -> WindowInfo {
        get_foreground_window_info()
    }

    fn send_key(&self, key: SyntheticKey) {
        let vk = match key {
            SyntheticKey::BrowserBack => 0xA6,    // VK_BROWSER_BACK
            SyntheticKey::BrowserForward => 0xA7, // VK_BROWSER_FORWARD
        };
        send_key_press(vk);
    }

    fn send_media(&self, cmd: MediaCommand) {
        let vk = match cmd {
            MediaCommand::PlayPause => 0xB3,  // VK_MEDIA_PLAY_PAUSE
            MediaCommand::Next => 0xB0,       // VK_MEDIA_NEXT_TRACK
            MediaCommand::Previous => 0xB1,   // VK_MEDIA_PREV_TRACK
            MediaCommand::Stop => 0xB2,       // VK_MEDIA_STOP
            MediaCommand::VolumeUp => 0xAF,   // VK_VOLUME_UP
            MediaCommand::VolumeDown => 0xAE, // VK_VOLUME_DOWN
            MediaCommand::VolumeMute => 0xAD, // VK_VOLUME_MUTE
        };
        send_key_press(vk);
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
        let keyboard_hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
            .map_err(|e| anyhow!("failed to install keyboard hook: {}", e))?;
        info!("keyboard hook installed");

        // Install low-level mouse hook
        let mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)
            .map_err(|e| anyhow!("failed to install mouse hook: {}", e))?;
        info!("mouse hook installed, starting message pump");

        // Message pump - required for low-level hooks to work
        // Exits when WM_QUIT is received (GetMessageW returns false)
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup (won't reach here normally)
        let _ = UnhookWindowsHookEx(keyboard_hook);
        let _ = UnhookWindowsHookEx(mouse_hook);
        info!("input hooks uninstalled");
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
    let key_event = KeyEvent::new(key_code, is_keydown);
    let input_event = InputEvent::Key(key_event);
    let should_block = process_hook_event(input_event);

    if should_block {
        // Return non-zero to block the key from propagating
        LRESULT(1)
    } else {
        // SAFETY: Windows requires us to call the next hook
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

/// Send event to main thread and wait for response
fn process_hook_event(event: InputEvent) -> bool {
    let Some(tx) = HOOK_CHANNEL.get() else {
        return false;
    };

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

/// Low-level mouse hook callback
/// SAFETY: Called by Windows from the message pump thread
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // code < 0 means we must pass to next hook without processing
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // Only process mouse wheel events
    if wparam.0 as u32 != WM_MOUSEWHEEL {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // SAFETY: lparam points to a valid MSLLHOOKSTRUCT when code >= 0
    let mouse_struct = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };

    // Extract wheel delta from mouseData (high word)
    // Positive = scroll up (away from user), negative = scroll down (toward user)
    let wheel_delta = (mouse_struct.mouseData >> 16) as i16;
    let scroll_up = wheel_delta > 0;

    trace!(wheel_delta, scroll_up, "hook received scroll event");

    // Try to send event to main thread and wait for response
    let input_event = InputEvent::Scroll { up: scroll_up };
    let should_block = process_hook_event(input_event);

    if should_block {
        // Return non-zero to block the scroll from propagating
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
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
