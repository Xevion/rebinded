//! Windows-specific implementation using Win32 low-level keyboard hooks
//!
//! Architecture:
//! - Low-level keyboard hook runs on a dedicated thread (Win32 requires message pump)
//! - Hook callback checks F13-F24, queries window info, resolves action
//! - Actions are executed synchronously in the hook (must be fast to avoid input lag)
//!
//! Key Win32 APIs:
//! - SetWindowsHookExW(WH_KEYBOARD_LL) for intercepting keys
//! - GetForegroundWindow + GetWindowTextW for window title
//! - GetClassNameW for window class
//! - GetWindowThreadProcessId + OpenProcess + QueryFullProcessImageNameW for binary

use crate::config::{Action, Config, WindowInfo};
use crate::state::{DebounceManager, DebounceResult};
use anyhow::{anyhow, Result};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::Mutex;
use std::sync::OnceLock;
use tracing::{debug, error, info, trace, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetMessageW,
    GetWindowTextW, GetWindowThreadProcessId, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

// Global state - Win32 hooks require static access from callback
static STATE: OnceLock<Mutex<HookState>> = OnceLock::new();

struct HookState {
    config: Config,
    debounce: DebounceManager,
}

/// Virtual key codes for F13-F24
const VK_F13: u32 = 0x7C;
const VK_F24: u32 = 0x87;

/// Media virtual key codes
const VK_MEDIA_NEXT_TRACK: u16 = 0xB0;
const VK_MEDIA_PREV_TRACK: u16 = 0xB1;
const VK_MEDIA_STOP: u16 = 0xB2;
const VK_MEDIA_PLAY_PAUSE: u16 = 0xB3;
const VK_BROWSER_BACK: u16 = 0xA6;
const VK_BROWSER_FORWARD: u16 = 0xA7;

pub async fn run(config: Config) -> Result<()> {
    info!("initializing Windows keyboard hook");

    // Initialize debounce manager from config
    let mut debounce = DebounceManager::new();
    for (name, profile) in &config.debounce {
        debounce.add_profile(name.clone(), profile.clone());
    }
    for (key, binding) in &config.bindings {
        if let Some(ref profile_name) = binding.debounce {
            debounce.set_key_profile(key.clone(), profile_name.clone());
        }
    }

    // Store in global state
    STATE
        .set(Mutex::new(HookState { config, debounce }))
        .map_err(|_| anyhow!("hook state already initialized"))?;

    // Run the hook on a blocking thread (Win32 message pump must be on same thread as hook)
    let handle = tokio::task::spawn_blocking(|| run_hook_thread());

    handle.await??;
    Ok(())
}

/// Runs the Win32 message pump - must be called from a dedicated thread
fn run_hook_thread() -> Result<()> {
    unsafe {
        // Install low-level keyboard hook
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
            .map_err(|e| anyhow!("failed to install keyboard hook: {}", e))?;

        info!("keyboard hook installed, starting message pump");

        // Message pump - required for low-level hooks to work
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
unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // code < 0 means we must pass to next hook without processing
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let kb_struct = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = kb_struct.vkCode;

    // Only process F13-F24
    if vk < VK_F13 || vk > VK_F24 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let is_keydown = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    let is_keyup = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);

    if !is_keydown && !is_keyup {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let key_name = vk_to_name(vk);
    trace!(vk, key_name, is_keydown, "hook received key event");

    // Process through our handler
    let should_block = process_key_event(key_name, vk, is_keydown);

    if should_block {
        // Return non-zero to block the key from propagating
        LRESULT(1)
    } else {
        // Pass to next hook
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Process a key event and determine if it should be blocked
/// Returns true if the key should be blocked (not passed to applications)
fn process_key_event(key_name: &str, _vk: u32, is_keydown: bool) -> bool {
    let Some(state) = STATE.get() else {
        warn!("hook state not initialized");
        return false;
    };

    let mut state = match state.lock() {
        Ok(s) => s,
        Err(e) => {
            error!("failed to lock hook state: {}", e);
            return false;
        }
    };

    // Process through debounce state machine
    let debounce_result = if is_keydown {
        state.debounce.key_down(key_name)
    } else {
        state.debounce.key_up(key_name)
    };

    match debounce_result {
        DebounceResult::Passthrough => {
            // No debounce configured, check config directly
            if !is_keydown {
                // We typically only act on key-down for non-debounced keys
                return false;
            }
            process_action(&state.config, key_name)
        }
        DebounceResult::Activate => {
            debug!(key_name, "debounce activated");
            process_action(&state.config, key_name)
        }
        DebounceResult::Suppress => {
            trace!(key_name, "debounce suppressed");
            true // Block the key
        }
    }
}

/// Look up and execute the action for a key
/// Returns true if the key should be blocked
fn process_action(config: &Config, key_name: &str) -> bool {
    let window_info = get_foreground_window_info();
    debug!(?window_info, key_name, "resolving action");

    let action = config.resolve_action(key_name, &window_info);

    match action {
        Some(Action::Passthrough) | None => {
            debug!(key_name, "passthrough");
            false // Don't block
        }
        Some(Action::Block) => {
            debug!(key_name, "blocked");
            true
        }
        Some(action) => {
            debug!(key_name, ?action, "executing action");
            execute_action_sync(action);
            true // Block original key
        }
    }
}

/// Execute an action synchronously (we're in the hook callback, can't await)
fn execute_action_sync(action: &Action) {
    match action {
        Action::MediaPlayPause => send_media_key(VK_MEDIA_PLAY_PAUSE),
        Action::MediaNext => send_media_key(VK_MEDIA_NEXT_TRACK),
        Action::MediaPrev => send_media_key(VK_MEDIA_PREV_TRACK),
        Action::MediaStop => send_media_key(VK_MEDIA_STOP),
        Action::BrowserBack => send_media_key(VK_BROWSER_BACK),
        Action::BrowserForward => send_media_key(VK_BROWSER_FORWARD),
        Action::Passthrough | Action::Block => {}
    }
}

/// Send a virtual key press using SendInput
fn send_media_key(vk: u16) {
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
                        dwExtraInfo: 0,
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
                        dwExtraInfo: 0,
                    },
                },
            },
        ];

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != 2 {
            warn!(vk, sent, "SendInput did not send all events");
        } else {
            trace!(vk, "sent media key");
        }
    }
}

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
    let len = GetWindowTextW(hwnd, &mut buffer);
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
    let len = GetClassNameW(hwnd, &mut buffer);
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
    use windows::core::PWSTR;

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return String::new();
    }

    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return String::new();
    };

    let mut buffer = [0u16; 512];
    let mut size = buffer.len() as u32;

    if QueryFullProcessImageNameW(process, PROCESS_NAME_FORMAT(0), PWSTR(buffer.as_mut_ptr()), &mut size).is_ok() {
        let path = OsString::from_wide(&buffer[..size as usize])
            .to_string_lossy()
            .into_owned();

        // Extract just the filename
        path.rsplit('\\').next().unwrap_or(&path).to_string()
    } else {
        String::new()
    }
}

/// Convert a Windows virtual key code to our key name
fn vk_to_name(vk: u32) -> &'static str {
    match vk {
        0x7C => "f13",
        0x7D => "f14",
        0x7E => "f15",
        0x7F => "f16",
        0x80 => "f17",
        0x81 => "f18",
        0x82 => "f19",
        0x83 => "f20",
        0x84 => "f21",
        0x85 => "f22",
        0x86 => "f23",
        0x87 => "f24",
        _ => "unknown",
    }
}
