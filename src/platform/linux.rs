//! Linux-specific platform implementation
//!
//! Key components:
//! - evdev for raw input device access and virtual device creation
//! - X11 (via x11rb) for window queries
//! - D-Bus (via zbus) for MPRIS media control and PulseAudio volume

use super::{EventResponse, MediaCommand, PlatformInterface, SyntheticKey};
use crate::config::WindowInfo;
use crate::key::{InputEvent, KeyCode, KeyEvent};
use crate::strategy::PlatformHandle;
use anyhow::{Context, Result, anyhow};
use evdev::uinput::VirtualDevice;
use evdev::{Device, EventType, RelativeAxisCode};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc as StdArc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, trace, warn};

// ============================================================================
// Key Name Resolution
// ============================================================================

/// Get human-readable key name from Linux evdev code
pub fn get_key_name(code: u32) -> String {
    if code > u16::MAX as u32 {
        return format!("UNKNOWN_{:#06X}", code);
    }
    format!("{:?}", evdev::KeyCode::new(code as u16))
}

/// Build reverse lookup map: name -> evdev code
pub fn build_key_name_map() -> HashMap<String, u32> {
    let mut map = HashMap::new();

    // Probe evdev key range (0-767 covers all standard keys)
    for code in 0..768u32 {
        let name = get_key_name(code);
        if !name.starts_with("UNKNOWN") {
            let normalized = name.to_lowercase();
            map.insert(normalized.clone(), code);

            // Strip "KEY_" prefix for convenience: "KEY_F13" -> "f13"
            if let Some(short) = normalized.strip_prefix("key_") {
                map.insert(short.to_string(), code);
            }
            // Strip "BTN_" prefix for buttons
            if let Some(short) = normalized.strip_prefix("btn_") {
                map.insert(short.to_string(), code);
            }
        }
    }

    map
}

// ============================================================================
// Platform Implementation
// ============================================================================

/// Linux platform implementation
pub struct Platform {
    /// X11 connection for window queries (lazy-initialized)
    x11_conn: Option<StdArc<Mutex<X11Connection>>>,
    /// D-Bus connection for media control (lazy-initialized)
    dbus_conn: Option<StdArc<zbus::Connection>>,
    /// Virtual keyboard device for key injection (lazy-initialized)
    /// Uses std::sync::Mutex (not tokio) to ensure synchronous, ordered event emission
    uinput_device: Option<StdArc<StdMutex<VirtualDevice>>>,
    /// MPRIS player state tracker for smart player selection
    mpris_tracker: StdArc<Mutex<MprisPlayerTracker>>,
}

/// X11 connection wrapper
struct X11Connection {
    conn: x11rb::rust_connection::RustConnection,
    screen_num: usize,
}

impl Default for Platform {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformInterface for Platform {
    fn new() -> Self {
        Self {
            x11_conn: None,
            dbus_conn: None,
            uinput_device: None,
            mpris_tracker: StdArc::new(Mutex::new(MprisPlayerTracker::new())),
        }
    }

    async fn run<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(InputEvent, PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>,
    {
        info!("starting Linux input handler");

        // Check permissions early with helpful error messages
        check_permissions()?;

        // Set up panic hook to ungrab devices on crash
        setup_panic_hook();

        // Find and grab all keyboard devices
        let devices = find_keyboard_devices().await?;
        if devices.is_empty() {
            return Err(anyhow!("no keyboard devices found"));
        }

        info!("found {} keyboard device(s)", devices.len());

        let mut grabbed_devices = Vec::new();
        for path in devices {
            match grab_device(&path).await {
                Ok(device) => {
                    info!(
                        "grabbed device: {} ({})",
                        device.name().unwrap_or("unknown"),
                        path.display()
                    );
                    grabbed_devices.push((path, device));
                }
                Err(e) => {
                    warn!("failed to grab {:?}: {}", path, e);
                }
            }
        }

        if grabbed_devices.is_empty() {
            return Err(anyhow!("failed to grab any keyboard devices"));
        }

        // Create virtual device for re-injection
        let uinput = create_virtual_keyboard().await?;
        self.uinput_device = Some(StdArc::new(StdMutex::new(uinput)));
        info!("created virtual keyboard for re-injection");

        // Create event channel for merging device streams
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<(evdev::InputEvent, PathBuf)>();

        // Spawn task for each device
        let mut device_tasks = Vec::new();
        for (path, device) in grabbed_devices {
            let tx = event_tx.clone();
            let task = tokio::spawn(async move {
                if let Err(e) = process_device_events(device, path.clone(), tx).await {
                    warn!("device {} stopped: {}", path.display(), e);
                }
            });
            device_tasks.push(task);
        }
        drop(event_tx); // Drop original sender so channel closes when all tasks exit

        // Spawn MPRIS focus monitor task
        // This tracks which media player windows are focused for smarter player selection
        let tracker = StdArc::clone(&self.mpris_tracker);
        let x11_conn = self.x11_conn.as_ref().map(StdArc::clone);
        tokio::spawn(async move {
            mpris_focus_monitor(x11_conn, tracker).await;
        });

        // Create platform handle for handler
        let platform_handle = PlatformHandle::new(self);

        // Set up XInput2 scroll handling (for scroll wheel bindings without grabbing mouse)
        let (mut scroll_rx, replay_tx) = match setup_xinput2_scroll_grab() {
            Ok((rx, tx)) => {
                info!("XInput2 scroll grab active - scroll wheel bindings enabled");
                (Some(rx), Some(tx))
            }
            Err(e) => {
                warn!(
                    "failed to set up XInput2 scroll grab: {}. Scroll wheel bindings will not work.",
                    e
                );
                (None, None)
            }
        };

        // Process events from both evdev (keyboard) and XInput2 (scroll)
        // Mouse movement is not grabbed, so it goes directly through the physical device
        loop {
            tokio::select! {
                // Handle keyboard events from evdev
                Some((raw_event, _device_path)) = event_rx.recv() => {
                    // Only process KEY events
                    if raw_event.event_type() != EventType::KEY {
                        continue;
                    }

                    // Convert evdev InputEvent to our InputEvent
                    let Some(input_event) = convert_event(&raw_event) else {
                        continue;
                    };

                    trace!(?input_event, "processing keyboard event");

                    // Call user handler
                    let response = handler(input_event, platform_handle).await;

                    // Re-inject if passthrough
                    if response == EventResponse::Passthrough
                        && let Some(ref uinput) = self.uinput_device
                    {
                        let mut dev = uinput.lock().unwrap();
                        if let Err(e) = dev.emit(&[raw_event]) {
                            warn!("failed to emit passthrough event: {}", e);
                        }
                    }
                }

                // Handle scroll events from XInput2
                Some(scroll_up) = async {
                    match &mut scroll_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let input_event = InputEvent::Scroll { up: scroll_up };
                    trace!(?input_event, "processing scroll event from XInput2");

                    // Call user handler
                    let response = handler(input_event, platform_handle).await;

                    // Send replay decision back to X11 thread
                    // If passthrough: replay the event (as if grab never happened)
                    // If blocked: do nothing (grab already consumed the event)
                    if let Some(ref tx) = replay_tx {
                        let should_replay = response == EventResponse::Passthrough;
                        if let Err(e) = tx.send(should_replay) {
                            warn!("failed to send scroll replay decision: {}", e);
                        }
                    }
                }

                // Exit when all event sources are closed
                else => {
                    info!("all event streams closed");
                    break;
                }
            }
        }

        info!("all device streams closed, shutting down");
        Ok(())
    }

    fn get_active_window(&self) -> WindowInfo {
        // Try to get window info via X11, fall back to empty on any error
        match get_x11_window_info(self.x11_conn.as_ref()) {
            Ok(info) => info,
            Err(e) => {
                warn_once!(
                    "X11 window query failed: {}. Window conditions will not work.",
                    e
                );
                WindowInfo::default()
            }
        }
    }

    fn send_key(&self, key: SyntheticKey) {
        let uinput = match &self.uinput_device {
            Some(device) => StdArc::clone(device),
            None => {
                warn!("uinput device not initialized");
                return;
            }
        };

        // Map to key combinations
        let events = match key {
            SyntheticKey::BrowserBack => create_key_combo(&[
                (evdev::KeyCode::KEY_LEFTALT, true),
                (evdev::KeyCode::KEY_LEFT, true),
                (evdev::KeyCode::KEY_LEFT, false),
                (evdev::KeyCode::KEY_LEFTALT, false),
            ]),
            SyntheticKey::BrowserForward => create_key_combo(&[
                (evdev::KeyCode::KEY_LEFTALT, true),
                (evdev::KeyCode::KEY_RIGHT, true),
                (evdev::KeyCode::KEY_RIGHT, false),
                (evdev::KeyCode::KEY_LEFTALT, false),
            ]),
        };

        // Emit in separate task to avoid blocking the handler
        tokio::spawn(async move {
            let mut dev = uinput.lock().unwrap();
            if let Err(e) = dev.emit(&events) {
                warn!("failed to emit synthetic key: {}", e);
            } else {
                debug!(?key, "emitted synthetic key");
            }
        });
    }

    fn send_media(&self, cmd: MediaCommand) {
        // Clone the D-Bus connection (will be lazy-initialized on first use)
        let dbus_conn = self.dbus_conn.as_ref().map(StdArc::clone);
        let tracker = StdArc::clone(&self.mpris_tracker);

        // Capture current window info for smart player selection (before spawning)
        let window_info = self.get_active_window();

        tokio::spawn(async move {
            // Handle volume commands via uinput (XF86Audio* keys)
            match cmd {
                MediaCommand::VolumeUp | MediaCommand::VolumeDown | MediaCommand::VolumeMute => {
                    send_volume_command(cmd).await;
                    return;
                }
                _ => {}
            }

            // Handle media commands via MPRIS D-Bus with smart player selection
            if let Err(e) = send_mpris_command(dbus_conn, cmd, &window_info, tracker).await {
                warn!("media command {:?} failed: {}", cmd, e);
            }
        });
    }
}

// ============================================================================
// XInput2 Scroll Handling
// ============================================================================

/// Set up XInput2 passive grabs for scroll wheel buttons (4=up, 5=down)
/// Returns channels for scroll events (forward) and replay decisions (back)
fn setup_xinput2_scroll_grab()
-> Result<(mpsc::UnboundedReceiver<bool>, mpsc::UnboundedSender<bool>)> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xinput::{self, ConnectionExt as XInputExt, EventMask};
    use x11rb::protocol::xproto::GrabStatus;

    let (scroll_tx, scroll_rx) = mpsc::unbounded_channel::<bool>();
    let (replay_tx, mut replay_rx) = mpsc::unbounded_channel::<bool>();

    // Connect to X11
    let (conn, screen_num) =
        x11rb::connect(None).context("failed to connect to X11 for scroll grab")?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // Query XInput2 extension (need version 2.0+)
    let xi_info = conn
        .xinput_xi_query_version(2, 0)?
        .reply()
        .context("failed to query XInput2 version")?;
    info!(
        "XInput2 version {}.{}",
        xi_info.major_version, xi_info.minor_version
    );

    // Query all XInput2 devices and build whitelist of physical pointer devices
    // This prevents re-capturing our own re-injected scroll events from the virtual device
    let devices_reply = conn
        .xinput_xi_query_device(xinput::Device::ALL)?
        .reply()
        .context("failed to query XInput2 devices")?;

    let mut physical_pointer_ids: HashSet<u16> = HashSet::new();
    for info in &devices_reply.infos {
        // Only include slave pointer devices (physical input devices)
        if info.type_ == xinput::DeviceType::SLAVE_POINTER {
            let name = String::from_utf8_lossy(&info.name);
            // Exclude virtual devices by name pattern
            if !name.contains("Virtual") && !name.contains("XTEST") && !name.contains("rebinded") {
                physical_pointer_ids.insert(info.deviceid);
                debug!(
                    "Whitelisted physical pointer: {} (id={})",
                    name.trim_end_matches('\0'),
                    info.deviceid
                );
            }
        }
    }
    info!(
        "Found {} physical pointer device(s) for scroll filtering",
        physical_pointer_ids.len()
    );

    // CRITICAL: Select events on root window BEFORE setting up grabs
    // Without this, the X11 server won't deliver any events to us
    let mask = xinput::XIEventMask::BUTTON_PRESS | xinput::XIEventMask::BUTTON_RELEASE;
    let event_mask = EventMask {
        deviceid: xinput::Device::ALL.into(),
        mask: vec![mask],
    };
    conn.xinput_xi_select_events(root, &[event_mask])?;
    debug!("XInput2 event selection registered on root window");

    // Set up passive grabs for buttons 4 (scroll up) and 5 (scroll down)
    // Use device ID 2 (VCP - Virtual Core Pointer) for passive grabs
    let vcp_device_id: u16 = 2;
    for button in [4u32, 5u32] {
        let result = conn.xinput_xi_passive_grab_device(
            x11rb::CURRENT_TIME,
            root,
            0, // no cursor change
            button,
            vcp_device_id,
            xinput::GrabType::BUTTON,
            xinput::GrabMode22::ASYNC,
            x11rb::protocol::xproto::GrabMode::ASYNC,
            xinput::GrabOwner::OWNER,
            &[u32::from(mask)],
            &[0], // any modifier
        )?;
        let reply = result.reply()?;
        if !reply.modifiers.is_empty() && reply.modifiers[0].status != GrabStatus::SUCCESS {
            warn!(
                "failed to grab button {}: {:?}",
                button, reply.modifiers[0].status
            );
        } else {
            debug!("passive grab set up for button {}", button);
        }
    }
    conn.flush()?;

    // Spawn blocking thread to handle X11 events and replay decisions
    std::thread::spawn(move || {
        loop {
            // Non-blocking check for replay decisions first
            while let Ok(should_replay) = replay_rx.try_recv() {
                if should_replay {
                    // Replay the event using XIAllowEvents with REPLAY_DEVICE mode
                    // This causes the frozen event to be replayed as if the grab never happened
                    use x11rb::protocol::xinput::{ConnectionExt as _, EventMode};
                    if let Err(e) = conn.xinput_xi_allow_events(
                        x11rb::CURRENT_TIME,
                        xinput::Device::ALL,
                        EventMode::REPLAY_DEVICE,
                        0, // touchid (not used for button events)
                        x11rb::NONE,
                    ) {
                        warn!("failed to replay scroll event: {}", e);
                    }
                } else {
                    // Event was blocked, grab already consumed it - nothing to do
                }
            }

            // Wait for next X11 event
            match conn.wait_for_event() {
                Ok(event) => {
                    // Handle scroll button presses (buttons 4=up, 5=down)
                    // Filter by sourceid to only accept events from physical devices,
                    // preventing infinite loops from previously re-injected events
                    let scroll_up = match &event {
                        x11rb::protocol::Event::XinputButtonPress(ev)
                            if ev.detail == 4 && physical_pointer_ids.contains(&ev.sourceid) =>
                        {
                            Some(true)
                        }
                        x11rb::protocol::Event::XinputButtonPress(ev)
                            if ev.detail == 5 && physical_pointer_ids.contains(&ev.sourceid) =>
                        {
                            Some(false)
                        }
                        _ => None,
                    };

                    if let Some(up) = scroll_up
                        && scroll_tx.send(up).is_err()
                    {
                        break; // Channel closed
                    }
                }
                Err(e) => {
                    warn!("X11 event error: {}", e);
                    break;
                }
            }
        }
    });

    Ok((scroll_rx, replay_tx))
}

// ============================================================================
// Device Management
// ============================================================================

/// Find keyboard input devices (excluding mice to avoid virtual device sensitivity issues)
///
/// We only grab keyboards via evdev. Scroll wheel events are intercepted via XInput2
/// at the X11 level, so we don't need to grab mouse devices.
async fn find_keyboard_devices() -> Result<Vec<PathBuf>> {
    let mut devices = Vec::new();

    for entry in std::fs::read_dir("/dev/input").context("failed to read /dev/input directory")? {
        let entry = entry?;
        let path = entry.path();

        // Only check event* devices
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !filename.starts_with("event") {
            continue;
        }

        // Try to open device to check capabilities
        let Ok(device) = Device::open(&path) else {
            continue;
        };

        // Check if device has keyboard capability (letter keys or function keys)
        let has_keyboard = device
            .supported_keys()
            .map(|keys| {
                keys.contains(evdev::KeyCode::KEY_A) || keys.contains(evdev::KeyCode::KEY_F1)
            })
            .unwrap_or(false);

        // Skip devices with mouse motion (REL_X/REL_Y) - these would cause sensitivity issues
        // when passed through a virtual device due to libinput's DPI handling
        let has_mouse_motion = device
            .supported_relative_axes()
            .map(|axes| axes.contains(RelativeAxisCode::REL_X))
            .unwrap_or(false);

        if has_keyboard && !has_mouse_motion {
            devices.push(path);
        }
    }

    Ok(devices)
}

/// Grab a device for exclusive access
async fn grab_device(path: &Path) -> Result<Device> {
    let mut device =
        Device::open(path).with_context(|| format!("failed to open device: {}", path.display()))?;

    device
        .grab()
        .with_context(|| format!("failed to grab device: {}", path.display()))?;

    Ok(device)
}

/// Process events from a single device
async fn process_device_events(
    device: Device,
    device_path: PathBuf,
    event_tx: mpsc::UnboundedSender<(evdev::InputEvent, PathBuf)>,
) -> Result<()> {
    let mut stream = device.into_event_stream()?;

    loop {
        match stream.next_event().await {
            Ok(event) => {
                if event_tx.send((event, device_path.clone())).is_err() {
                    // Channel closed, exit
                    break;
                }
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    Ok(())
}

/// Convert evdev InputEvent to our InputEvent type
fn convert_event(ev: &evdev::InputEvent) -> Option<InputEvent> {
    match ev.event_type() {
        EventType::KEY => {
            let key_code = KeyCode::new(ev.code() as u32);
            // value: 1 = press, 0 = release, 2 = auto-repeat
            // We only care about press (1) and release (0), ignore auto-repeat
            let down = ev.value() == 1;

            // Filter out auto-repeat events
            if ev.value() == 2 {
                return None;
            }

            Some(InputEvent::Key(KeyEvent::new(key_code, down)))
        }
        EventType::RELATIVE => {
            // REL_WHEEL: value > 0 = up (away from user), value < 0 = down (toward user)
            if ev.code() == RelativeAxisCode::REL_WHEEL.0 {
                let up = ev.value() > 0;
                trace!(
                    "scroll event captured: {} (value={})",
                    if up { "up" } else { "down" },
                    ev.value()
                );
                Some(InputEvent::Scroll { up })
            } else {
                None // Ignore other relative axes (mouse movement, etc.)
            }
        }
        _ => None, // Ignore all other event types
    }
}

// ============================================================================
// Virtual Device (uinput)
// ============================================================================

/// Create a virtual keyboard for re-injecting events
///
/// Note: We intentionally DON'T include mouse axes (REL_X/REL_Y) because libinput
/// applies different DPI handling to virtual devices, causing sensitivity issues.
/// Mouse events go directly through the physical device (not grabbed).
/// Scroll wheel is intercepted via XInput2 at the X11 level instead.
async fn create_virtual_keyboard() -> Result<VirtualDevice> {
    use evdev::AttributeSet;

    // Create key set with all standard keys (including mouse buttons)
    let mut keys = AttributeSet::<evdev::KeyCode>::new();
    for code in 0..=767u16 {
        keys.insert(evdev::KeyCode::new(code));
    }

    // Only include scroll wheel axes for re-injection (no mouse motion)
    let mut relative_axes = AttributeSet::<RelativeAxisCode>::new();
    relative_axes.insert(RelativeAxisCode::REL_WHEEL); // Scroll wheel
    relative_axes.insert(RelativeAxisCode::REL_HWHEEL); // Horizontal scroll

    let device = VirtualDevice::builder()?
        .name("rebinded-virtual-keyboard")
        .with_keys(&keys)?
        .with_relative_axes(&relative_axes)?
        .build()?;

    Ok(device)
}

/// Create a SYN_REPORT synchronization event
fn create_syn_report() -> evdev::InputEvent {
    evdev::InputEvent::new(
        evdev::EventType::SYNCHRONIZATION.0,
        0, // SYN_REPORT code
        0,
    )
}

/// Create a key combo as evdev InputEvents with proper synchronization
fn create_key_combo(keys: &[(evdev::KeyCode, bool)]) -> Vec<evdev::InputEvent> {
    let mut events = Vec::new();
    for (key, down) in keys {
        let value = if *down { 1 } else { 0 };
        events.push(evdev::InputEvent::new(EventType::KEY.0, key.0, value));
        events.push(create_syn_report());
    }
    events
}

/// Send volume command via pactl (PulseAudio/PipeWire)
///
/// Uses `pactl` command to directly control system volume.
/// This is more reliable than emitting XF86Audio keys, which may not be
/// recognized by all desktop environments or audio systems.
async fn send_volume_command(cmd: MediaCommand) {
    let pactl_arg = match cmd {
        MediaCommand::VolumeUp => "+2%",
        MediaCommand::VolumeDown => "-2%",
        MediaCommand::VolumeMute => "toggle",
        _ => return,
    };

    let pactl_cmd = match cmd {
        MediaCommand::VolumeMute => "set-sink-mute",
        _ => "set-sink-volume",
    };

    // Spawn pactl command
    let result = tokio::process::Command::new("pactl")
        .arg(pactl_cmd)
        .arg("@DEFAULT_SINK@")
        .arg(pactl_arg)
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            debug!(?cmd, "volume command executed successfully");
        }
        Ok(output) => {
            warn!(
                ?cmd,
                stderr = ?String::from_utf8_lossy(&output.stderr),
                "pactl command failed"
            );
        }
        Err(e) => {
            warn!(?cmd, error = ?e, "failed to execute pactl command");
        }
    }
}

// ============================================================================
// X11 Window Queries
// ============================================================================

/// Get window information via X11
fn get_x11_window_info(x11_conn: Option<&StdArc<Mutex<X11Connection>>>) -> Result<WindowInfo> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    // Get or create X11 connection
    let conn_arc = match x11_conn {
        Some(c) => StdArc::clone(c),
        None => {
            // Lazy initialization - will fail if X11 not available
            let (conn, screen_num) = x11rb::connect(None).context("failed to connect to X11")?;
            StdArc::new(Mutex::new(X11Connection { conn, screen_num }))
        }
    };

    // Block on the async lock (we're in a sync context from Platform::get_active_window)
    let guard = match conn_arc.try_lock() {
        Ok(g) => g,
        Err(_) => {
            // If we can't get the lock immediately, return empty rather than blocking
            return Ok(WindowInfo::default());
        }
    };

    let conn = &guard.conn;
    let root = conn.setup().roots[guard.screen_num].root;

    // Get active window atom
    let net_active_window = intern_atom_cached(conn, "_NET_ACTIVE_WINDOW")?;
    let window_atom = intern_atom_cached(conn, "WINDOW")?;

    // Query active window ID
    let reply = conn
        .get_property(false, root, net_active_window, window_atom, 0, 1)?
        .reply()?;

    if reply.value.len() < 4 {
        return Ok(WindowInfo::default());
    }

    let active_window = u32::from_ne_bytes([
        reply.value[0],
        reply.value[1],
        reply.value[2],
        reply.value[3],
    ]);

    // Query window properties
    Ok(WindowInfo {
        title: get_x11_window_title(conn, active_window).unwrap_or_default(),
        class: get_x11_window_class(conn, active_window).unwrap_or_default(),
        binary: get_x11_window_binary(conn, active_window).unwrap_or_default(),
    })
}

/// Intern an X11 atom (with caching)
fn intern_atom_cached(conn: &x11rb::rust_connection::RustConnection, name: &str) -> Result<u32> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let reply = conn.intern_atom(false, name.as_bytes())?.reply()?;
    Ok(reply.atom)
}

/// Get window title
fn get_x11_window_title(
    conn: &x11rb::rust_connection::RustConnection,
    window: u32,
) -> Result<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::xproto::*;

    // Try _NET_WM_NAME (UTF-8) first
    let net_wm_name = intern_atom_cached(conn, "_NET_WM_NAME")?;
    let utf8_string = intern_atom_cached(conn, "UTF8_STRING")?;

    let reply = conn
        .get_property(false, window, net_wm_name, utf8_string, 0, 1024)?
        .reply()?;

    if !reply.value.is_empty()
        && let Ok(s) = String::from_utf8(reply.value)
    {
        return Ok(s);
    }

    // Fallback to WM_NAME
    let wm_name: u32 = AtomEnum::WM_NAME.into();
    let string_atom: u32 = AtomEnum::STRING.into();

    let reply = conn
        .get_property(false, window, wm_name, string_atom, 0, 1024)?
        .reply()?;

    Ok(String::from_utf8_lossy(&reply.value).into_owned())
}

/// Get window class (second element of WM_CLASS)
fn get_x11_window_class(
    conn: &x11rb::rust_connection::RustConnection,
    window: u32,
) -> Result<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::xproto::*;

    let wm_class: u32 = AtomEnum::WM_CLASS.into();
    let string_atom: u32 = AtomEnum::STRING.into();

    let reply = conn
        .get_property(false, window, wm_class, string_atom, 0, 1024)?
        .reply()?;

    // WM_CLASS format: "instance\0class\0"
    let s = String::from_utf8_lossy(&reply.value);
    Ok(s.split('\0').nth(1).unwrap_or("").to_string())
}

/// Get window binary name via PID
fn get_x11_window_binary(
    conn: &x11rb::rust_connection::RustConnection,
    window: u32,
) -> Result<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    // Get _NET_WM_PID
    let net_wm_pid = intern_atom_cached(conn, "_NET_WM_PID")?;
    let cardinal = intern_atom_cached(conn, "CARDINAL")?;

    let reply = conn
        .get_property(false, window, net_wm_pid, cardinal, 0, 1)?
        .reply()?;

    if reply.value.len() < 4 {
        return Ok(String::new());
    }

    let pid = u32::from_ne_bytes([
        reply.value[0],
        reply.value[1],
        reply.value[2],
        reply.value[3],
    ]);

    // Read /proc/<pid>/exe symlink
    let exe_path = std::fs::read_link(format!("/proc/{}/exe", pid))?;
    Ok(exe_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default())
}

// ============================================================================
// D-Bus / MPRIS Media Control
// ============================================================================

/// Information about an MPRIS media player
#[derive(Debug)]
struct MprisPlayerInfo {
    /// D-Bus service name (e.g., "org.mpris.MediaPlayer2.spotify")
    service_name: String,
    /// Human-readable identity (e.g., "Spotify", "Firefox")
    identity: String,
    /// Current playback status: "Playing", "Paused", or "Stopped"
    playback_status: String,
}

impl MprisPlayerInfo {
    /// Check if this player is currently playing media
    fn is_playing(&self) -> bool {
        self.playback_status == "Playing"
    }

    /// Extract the player name from the service name
    /// e.g., "org.mpris.MediaPlayer2.spotify" -> "spotify"
    /// e.g., "org.mpris.MediaPlayer2.firefox.instance_1234" -> "firefox"
    fn player_name(&self) -> &str {
        const PREFIX: &str = "org.mpris.MediaPlayer2.";
        let name = self
            .service_name
            .strip_prefix(PREFIX)
            .unwrap_or(&self.service_name);
        // Handle instance suffixes like "firefox.instance_1234"
        name.split('.').next().unwrap_or(name)
    }

    /// Check if this player matches the given window info
    /// Matches against window binary name and class (case-insensitive)
    fn matches_window(&self, window: &WindowInfo) -> bool {
        let player_name = self.player_name().to_lowercase();
        let identity = self.identity.to_lowercase();

        // Extract just the binary name from full path (e.g., "/usr/bin/firefox" -> "firefox")
        let binary_name = window
            .binary
            .rsplit('/')
            .next()
            .unwrap_or(&window.binary)
            .to_lowercase();
        let class = window.class.to_lowercase();

        // Helper to check bidirectional contains (only if both strings are non-empty)
        let matches = |a: &str, b: &str| -> bool {
            !a.is_empty() && !b.is_empty() && (a.contains(b) || b.contains(a))
        };

        // Check various matching strategies
        matches(&binary_name, &player_name)
            || matches(&class, &player_name)
            || matches(&binary_name, &identity)
            || matches(&class, &identity)
    }

    /// Check if this player's process family matches the window
    /// More lenient than matches_window - checks if they share a common base
    /// e.g., "vivaldi" matches "vivaldi-bin", "chromium" matches "chromium-browser"
    fn matches_process_family(&self, window: &WindowInfo) -> bool {
        let player_name = self.player_name().to_lowercase();

        // Extract binary base name
        let binary_name = window
            .binary
            .rsplit('/')
            .next()
            .unwrap_or(&window.binary)
            .to_lowercase();

        // Bail early if binary is empty - can't match process family without it
        if binary_name.is_empty() {
            return false;
        }

        // Strip common suffixes for comparison
        let binary_base = binary_name
            .strip_suffix("-bin")
            .or_else(|| binary_name.strip_suffix("-browser"))
            .or_else(|| binary_name.strip_suffix("-stable"))
            .unwrap_or(&binary_name);

        let player_base = player_name
            .strip_suffix("-bin")
            .or_else(|| player_name.strip_suffix("-browser"))
            .or_else(|| player_name.strip_suffix("-stable"))
            .unwrap_or(&player_name);

        // Check if bases match or one contains the other
        binary_base == player_base
            || binary_base.starts_with(player_base)
            || player_base.starts_with(binary_base)
    }
}

/// Tracks historical state for MPRIS player selection
///
/// This enables smarter player selection by remembering which players were
/// recently focused or playing, allowing media commands to target the "right"
/// player even when focused on an unrelated window.
#[derive(Debug, Default)]
struct MprisPlayerTracker {
    /// Player name -> last time window was focused (e.g., "spotify" -> Instant)
    last_focused: HashMap<String, Instant>,
    /// Player name -> last time player was in "Playing" state
    last_playing: HashMap<String, Instant>,
    /// Cached list of known MPRIS player names for window matching
    known_players: Vec<String>,
    /// Last time we refreshed the known players list
    last_player_refresh: Option<Instant>,
}

impl MprisPlayerTracker {
    /// Focus tracking expires after 10 minutes
    const FOCUS_EXPIRY: Duration = Duration::from_secs(10 * 60);
    /// How often to refresh the list of known MPRIS players
    const PLAYER_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self::default()
    }

    /// Record that a player's window was focused now
    fn record_focus(&mut self, player_name: &str) {
        self.last_focused
            .insert(player_name.to_lowercase(), Instant::now());
    }

    /// Record that a player was playing now
    fn record_playing(&mut self, player_name: &str) {
        self.last_playing
            .insert(player_name.to_lowercase(), Instant::now());
    }

    /// Get focus time if within expiry window, None otherwise
    fn get_valid_focus(&self, player_name: &str) -> Option<Instant> {
        self.last_focused
            .get(&player_name.to_lowercase())
            .copied()
            .filter(|t| t.elapsed() < Self::FOCUS_EXPIRY)
    }

    /// Get last playing time (never expires)
    fn get_last_playing(&self, player_name: &str) -> Option<Instant> {
        self.last_playing.get(&player_name.to_lowercase()).copied()
    }

    /// Check if the known players cache needs refreshing
    fn needs_player_refresh(&self) -> bool {
        self.last_player_refresh
            .map(|t| t.elapsed() >= Self::PLAYER_REFRESH_INTERVAL)
            .unwrap_or(true)
    }

    /// Update the cached list of known players
    fn update_known_players(&mut self, players: Vec<String>) {
        self.known_players = players;
        self.last_player_refresh = Some(Instant::now());
    }

    /// Find which player (if any) matches the given window
    fn find_matching_player(&self, window: &WindowInfo) -> Option<&str> {
        // Create temporary MprisPlayerInfo to use existing matching logic
        for player_name in &self.known_players {
            let temp_info = MprisPlayerInfo {
                service_name: format!("org.mpris.MediaPlayer2.{}", player_name),
                identity: player_name.clone(),
                playback_status: String::new(),
            };
            if temp_info.matches_window(window) || temp_info.matches_process_family(window) {
                return Some(player_name);
            }
        }
        None
    }
}

/// Background task that monitors window focus to track MPRIS player activity
///
/// Polls the active window every 500ms and records focus events for windows
/// that match known MPRIS players. Also periodically refreshes the list of
/// known players from D-Bus.
async fn mpris_focus_monitor(
    x11_conn: Option<StdArc<Mutex<X11Connection>>>,
    tracker: StdArc<Mutex<MprisPlayerTracker>>,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(500);

    // Create our own D-Bus connection for querying MPRIS players
    let dbus_conn = match zbus::Connection::session().await {
        Ok(conn) => conn,
        Err(e) => {
            warn!("focus monitor: failed to connect to D-Bus: {}", e);
            return;
        }
    };

    let mut last_focused_player: Option<String> = None;

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        // Get current active window
        let window_info = match get_x11_window_info(x11_conn.as_ref()) {
            Ok(info) if !info.binary.is_empty() || !info.class.is_empty() => info,
            _ => continue, // Skip if we can't get window info
        };

        let mut tracker_guard = tracker.lock().await;

        // Refresh known players list if needed
        if tracker_guard.needs_player_refresh()
            && let Some(players) = list_mpris_players(&dbus_conn).await
        {
            // Extract player names from service names
            let player_names: Vec<String> = players
                .iter()
                .filter_map(|s| {
                    s.strip_prefix("org.mpris.MediaPlayer2.")
                        .map(|name| name.split('.').next().unwrap_or(name).to_string())
                })
                .collect();
            tracker_guard.update_known_players(player_names);
        }

        // Check if current window matches any known player
        if let Some(player_name) = tracker_guard.find_matching_player(&window_info) {
            let player_name = player_name.to_string();

            // Only record if this is a new focus (avoid spamming updates)
            if last_focused_player.as_ref() != Some(&player_name) {
                debug!(
                    "focus changed to player: {} (window: {})",
                    player_name, window_info.class
                );
                tracker_guard.record_focus(&player_name);
                drop(tracker_guard); // Release lock before updating last_focused_player
                last_focused_player = Some(player_name);
            }
        } else {
            last_focused_player = None;
        }
    }
}

/// Send MPRIS media command with smart player selection
async fn send_mpris_command(
    dbus_conn: Option<StdArc<zbus::Connection>>,
    cmd: MediaCommand,
    window_info: &WindowInfo,
    tracker: StdArc<Mutex<MprisPlayerTracker>>,
) -> Result<()> {
    use zbus::proxy;

    // Get or create D-Bus connection
    let conn = match dbus_conn {
        Some(c) => c,
        None => StdArc::new(zbus::Connection::session().await?),
    };

    // Find the best MPRIS player based on playback state and window focus
    let player_name = find_best_mpris_player(&conn, window_info, &tracker)
        .await
        .context("no MPRIS media players found")?;

    debug!("sending MPRIS command {:?} to {}", cmd, player_name);

    // Define MPRIS Player interface
    #[proxy(
        interface = "org.mpris.MediaPlayer2.Player",
        default_service = "org.mpris.MediaPlayer2",
        default_path = "/org/mpris/MediaPlayer2"
    )]
    trait MediaPlayer2Player {
        async fn play_pause(&self) -> zbus::Result<()>;
        async fn next(&self) -> zbus::Result<()>;
        async fn previous(&self) -> zbus::Result<()>;
        async fn stop(&self) -> zbus::Result<()>;
    }

    // Create proxy for the player
    let proxy = MediaPlayer2PlayerProxy::builder(&conn)
        .destination(player_name)?
        .build()
        .await?;

    // Call appropriate method
    match cmd {
        MediaCommand::PlayPause => proxy.play_pause().await?,
        MediaCommand::Next => proxy.next().await?,
        MediaCommand::Previous => proxy.previous().await?,
        MediaCommand::Stop => proxy.stop().await?,
        _ => {}
    }

    Ok(())
}

/// Find the best MPRIS player based on priority:
/// 1. Currently playing media (highest priority)
/// 2. Matches the focused window (current)
/// 3. Same process family as focused window
/// 4. Last focused within 10 minutes (more recent wins)
/// 5. Last playing (more recent wins, never expires)
/// 6. Any available player (fallback)
async fn find_best_mpris_player(
    conn: &zbus::Connection,
    window_info: &WindowInfo,
    tracker: &StdArc<Mutex<MprisPlayerTracker>>,
) -> Option<String> {
    // Get all MPRIS player service names
    let player_services = list_mpris_players(conn).await?;

    if player_services.is_empty() {
        return None;
    }

    // Get detailed info for each player
    let mut players: Vec<MprisPlayerInfo> = Vec::new();
    for service in player_services {
        if let Some(info) = get_mpris_player_info(conn, &service).await {
            players.push(info);
        }
    }

    if players.is_empty() {
        return None;
    }

    // Update tracker with currently playing players
    {
        let mut tracker_guard = tracker.lock().await;
        for player in &players {
            if player.is_playing() {
                tracker_guard.record_playing(player.player_name());
            }
        }
    }

    // Get tracker state for selection (read-only from here)
    let tracker_guard = tracker.lock().await;

    debug!(
        "found {} MPRIS players, focused window: binary={:?} class={:?}",
        players.len(),
        window_info.binary,
        window_info.class
    );

    // Log each player's selection factors
    for player in &players {
        let player_name = player.player_name();
        debug!(
            "player {} (identity={}, playing={}, window_match={}, family_match={}, last_focus={:?}, last_playing={:?})",
            player.service_name,
            player.identity,
            player.is_playing(),
            player.matches_window(window_info),
            player.matches_process_family(window_info),
            tracker_guard
                .get_valid_focus(player_name)
                .map(|t| t.elapsed()),
            tracker_guard
                .get_last_playing(player_name)
                .map(|t| t.elapsed()),
        );
    }

    // Select the best player using comparison-based priority:
    // 1. Currently playing (highest)
    // 2. Matches focused window (current)
    // 3. Same process family as focused window
    // 4. Last focused within 10 min (more recent wins)
    // 5. Last playing (more recent wins)
    let best_player = players.iter().max_by(|a, b| {
        let a_name = a.player_name();
        let b_name = b.player_name();

        a.is_playing()
            .cmp(&b.is_playing())
            .then_with(|| {
                a.matches_window(window_info)
                    .cmp(&b.matches_window(window_info))
            })
            .then_with(|| {
                a.matches_process_family(window_info)
                    .cmp(&b.matches_process_family(window_info))
            })
            .then_with(|| {
                // Last focused within 10 min - more recent wins (larger Instant = more recent)
                tracker_guard
                    .get_valid_focus(a_name)
                    .cmp(&tracker_guard.get_valid_focus(b_name))
            })
            .then_with(|| {
                // Last playing - more recent wins (larger Instant = more recent)
                tracker_guard
                    .get_last_playing(a_name)
                    .cmp(&tracker_guard.get_last_playing(b_name))
            })
    });

    best_player.map(|p| {
        debug!("selected player: {}", p.service_name);
        p.service_name.clone()
    })
}

/// List all MPRIS media player D-Bus service names
async fn list_mpris_players(conn: &zbus::Connection) -> Option<Vec<String>> {
    use zbus::proxy;

    #[proxy(
        interface = "org.freedesktop.DBus",
        default_service = "org.freedesktop.DBus",
        default_path = "/org/freedesktop/DBus"
    )]
    trait DBus {
        fn list_names(&self) -> zbus::Result<Vec<String>>;
    }

    let proxy = DBusProxy::new(conn).await.ok()?;
    let names = proxy.list_names().await.ok()?;

    Some(
        names
            .into_iter()
            .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
            .collect(),
    )
}

/// Get detailed information about an MPRIS player
async fn get_mpris_player_info(conn: &zbus::Connection, service: &str) -> Option<MprisPlayerInfo> {
    use zbus::proxy;
    use zbus::zvariant::OwnedValue;

    #[proxy(
        interface = "org.freedesktop.DBus.Properties",
        default_path = "/org/mpris/MediaPlayer2"
    )]
    trait Properties {
        fn get(&self, interface: &str, property: &str) -> zbus::Result<OwnedValue>;
    }

    let proxy = PropertiesProxy::builder(conn)
        .destination(service)
        .ok()?
        .build()
        .await
        .ok()?;

    // Get Identity from org.mpris.MediaPlayer2 interface
    let identity = proxy
        .get("org.mpris.MediaPlayer2", "Identity")
        .await
        .ok()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default();

    // Get PlaybackStatus from org.mpris.MediaPlayer2.Player interface
    let playback_status = proxy
        .get("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
        .await
        .ok()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_else(|| "Stopped".to_string());

    Some(MprisPlayerInfo {
        service_name: service.to_string(),
        identity,
        playback_status,
    })
}

// ============================================================================
// Error Handling & Utilities
// ============================================================================

/// Check system permissions and requirements
fn check_permissions() -> Result<()> {
    // Check /dev/input exists
    if !Path::new("/dev/input").exists() {
        return Err(anyhow!("/dev/input not found. Are you running on Linux?"));
    }

    // Check if we can read at least one input device
    let readable = std::fs::read_dir("/dev/input")?
        .filter_map(|e| e.ok())
        .any(|e| {
            let path = e.path();
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("event"))
                .unwrap_or(false)
                && std::fs::File::open(&path).is_ok()
        });

    if !readable {
        return Err(anyhow!(
            "Cannot read /dev/input devices.\n\
            Add yourself to the 'input' group:\n  \
            sudo usermod -aG input $USER\n\
            Then log out and back in."
        ));
    }

    // Check uinput exists
    if !Path::new("/dev/uinput").exists() {
        return Err(anyhow!(
            "/dev/uinput not found. Load the uinput module:\n  \
            sudo modprobe uinput\n\n\
            To load automatically at boot:\n  \
            echo uinput | sudo tee /etc/modules-load.d/uinput.conf"
        ));
    }

    // Check if we can write to uinput
    if OpenOptions::new().write(true).open("/dev/uinput").is_err() {
        return Err(anyhow!(
            "Cannot write to /dev/uinput.\n\
            Create a udev rule:\n  \
            echo 'KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"' | \\\n    \
            sudo tee /etc/udev/rules.d/99-input.rules\n  \
            sudo udevadm control --reload-rules\n  \
            sudo udevadm trigger"
        ));
    }

    Ok(())
}

/// Set up panic hook to ungrab devices
fn setup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Try to ungrab all devices
        warn!("panic detected, attempting to ungrab devices");
        let _ = ungrab_all_devices();

        // Call original panic hook
        default_hook(panic_info);
    }));
}

/// Attempt to ungrab all devices (best effort)
fn ungrab_all_devices() -> Result<()> {
    for entry in std::fs::read_dir("/dev/input")? {
        let path = entry?.path();
        if let Some(filename) = path.file_name().and_then(|n| n.to_str())
            && filename.starts_with("event")
            && let Ok(mut device) = Device::open(&path)
        {
            let _ = device.ungrab();
        }
    }
    Ok(())
}

/// Macro for warning only once
macro_rules! warn_once {
    ($($arg:tt)*) => {{
        use std::sync::Once;
        static WARNED: Once = Once::new();
        WARNED.call_once(|| {
            warn!($($arg)*);
        });
    }};
}

// Export the macro for use in this module
use warn_once;
