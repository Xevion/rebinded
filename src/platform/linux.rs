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
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc as StdArc;
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
    uinput_device: Option<StdArc<Mutex<VirtualDevice>>>,
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

        info!("found {} input device(s) (keyboards + mice)", devices.len());

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
        self.uinput_device = Some(StdArc::new(Mutex::new(uinput)));
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

        // Create platform handle for handler
        let platform_handle = PlatformHandle::new(self);

        // Process merged event stream
        while let Some((raw_event, _device_path)) = event_rx.recv().await {
            // Fast path: Pass through mouse movement, buttons, and other events we don't handle
            // This keeps the mouse responsive even though it's grabbed
            let should_process =
                matches!(raw_event.event_type(), EventType::KEY | EventType::RELATIVE)
                    && (raw_event.event_type() != EventType::RELATIVE
                        || raw_event.code() == RelativeAxisCode::REL_WHEEL.0);

            if !should_process {
                // Immediately pass through unhandled events (mouse movement, buttons, etc.)
                if let Some(ref uinput) = self.uinput_device {
                    let device = StdArc::clone(uinput);
                    let event = raw_event;
                    tokio::spawn(async move {
                        let mut dev = device.lock().await;
                        let _ = dev.emit(&[event]);
                    });
                }
                continue;
            }

            // Convert evdev InputEvent to our InputEvent
            let Some(input_event) = convert_event(&raw_event) else {
                continue; // Shouldn't happen given the filter above, but be safe
            };

            trace!(?input_event, "processing event");

            // Call user handler
            let response = handler(input_event, platform_handle).await;

            // Re-inject if passthrough
            if response == EventResponse::Passthrough
                && let Some(ref uinput) = self.uinput_device
            {
                let device = StdArc::clone(uinput);
                let event = raw_event;
                tokio::spawn(async move {
                    let mut dev = device.lock().await;
                    if let Err(e) = dev.emit(&[event]) {
                        warn!("failed to emit passthrough event: {}", e);
                    }
                });
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

        // Emit in separate task to avoid blocking
        tokio::spawn(async move {
            let mut dev = uinput.lock().await;
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

        tokio::spawn(async move {
            // Handle volume commands via uinput (XF86Audio* keys)
            match cmd {
                MediaCommand::VolumeUp | MediaCommand::VolumeDown | MediaCommand::VolumeMute => {
                    send_volume_command(cmd).await;
                    return;
                }
                _ => {}
            }

            // Handle media commands via MPRIS D-Bus
            if let Err(e) = send_mpris_command(dbus_conn, cmd).await {
                warn!("media command {:?} failed: {}", cmd, e);
            }
        });
    }
}

// ============================================================================
// Device Management
// ============================================================================

/// Find all keyboard and mouse input devices
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

        // Check if device has keyboard capability OR scroll wheel
        // Keyboard: Must have at least some letter keys or function keys
        let has_keyboard = device
            .supported_keys()
            .map(|keys| {
                keys.contains(evdev::KeyCode::KEY_A) || keys.contains(evdev::KeyCode::KEY_F1)
            })
            .unwrap_or(false);

        // Mouse: Must have REL_WHEEL for scroll events
        let has_scroll = device
            .supported_relative_axes()
            .map(|axes| axes.contains(RelativeAxisCode::REL_WHEEL))
            .unwrap_or(false);

        if has_keyboard || has_scroll {
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
async fn create_virtual_keyboard() -> Result<VirtualDevice> {
    use evdev::AttributeSet;

    // Create key set with all standard keys (including mouse buttons)
    let mut keys = AttributeSet::<evdev::KeyCode>::new();
    for code in 0..=767u16 {
        keys.insert(evdev::KeyCode::new(code));
    }

    // Create relative axis set for mouse movement and scroll wheel pass-through
    let mut relative_axes = AttributeSet::<RelativeAxisCode>::new();
    relative_axes.insert(RelativeAxisCode::REL_X); // Mouse X movement
    relative_axes.insert(RelativeAxisCode::REL_Y); // Mouse Y movement
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

/// Send MPRIS media command
async fn send_mpris_command(
    dbus_conn: Option<StdArc<zbus::Connection>>,
    cmd: MediaCommand,
) -> Result<()> {
    use zbus::proxy;

    // Get or create D-Bus connection
    let conn = match dbus_conn {
        Some(c) => c,
        None => StdArc::new(zbus::Connection::session().await?),
    };

    // Find active MPRIS player
    let player_name = find_mpris_player(&conn)
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

/// Find an active MPRIS media player
async fn find_mpris_player(conn: &zbus::Connection) -> Option<String> {
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

    names
        .into_iter()
        .find(|name| name.starts_with("org.mpris.MediaPlayer2."))
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
