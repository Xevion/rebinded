//! Linux-specific platform implementation
//!
//! Key components:
//! - evdev for raw input device access and virtual device creation
//! - X11 (via x11rb) for window queries
//! - D-Bus (via zbus) for MPRIS media control
//!
//! Scroll is intercepted at the evdev layer rather than through an X11 button
//! grab. A grab can only see the emulated button 4/5 events, while toolkits
//! scroll from the high resolution axis those are derived from, so a grab
//! cannot suppress a tick it has already been delivered.
//!
//! One dedicated thread owns the X11 connection and publishes the focused
//! window, so no blocking round trip lands on the tokio runtime that processes
//! input. `get_active_window` reads a cached snapshot.

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
use std::sync::{Arc as StdArc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OnceCell, mpsc, watch};
use tracing::{debug, error, info, trace, warn};

/// How long to wait before the first X11 reconnect attempt.
const X11_RECONNECT_MIN: Duration = Duration::from_secs(1);
/// Ceiling for the X11 reconnect backoff.
const X11_RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Backstop for devices inotify missed, or that were busy when last tried.
const DEVICE_RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Name of the uinput device we re-inject through, and the name we refuse to
/// grab so our own output cannot feed back in.
const VIRTUAL_KEYBOARD_NAME: &str = "rebinded-virtual-keyboard";

/// What a claimed device is held for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeviceRole {
    /// Claimed for its keys; only key events are re-injected.
    Keyboard,
    /// Claimed for its wheel; every event is mirrored except diverted ticks.
    Pointer,
}

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

/// The focused-window snapshot published by the X11 thread.
///
/// Poisoning is recovered from so a panic elsewhere cannot permanently
/// disable window conditions.
#[derive(Default)]
struct WindowState {
    info: StdRwLock<WindowInfo>,
}

impl WindowState {
    fn get(&self) -> WindowInfo {
        self.info
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, next: WindowInfo) {
        *self
            .info
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }
}

/// Linux platform implementation
pub struct Platform {
    /// Focused-window snapshot, republished by the X11 thread
    window_state: StdArc<WindowState>,
    /// Session bus connection, established once on first use and reused
    dbus_conn: StdArc<OnceCell<zbus::Connection>>,
    /// Virtual keyboard device for key injection
    /// Uses std::sync::Mutex (not tokio) to ensure synchronous, ordered event emission
    uinput_device: Option<StdArc<StdMutex<VirtualDevice>>>,
    /// MPRIS player state tracker for smart player selection
    mpris_tracker: StdArc<Mutex<MprisPlayerTracker>>,
}

/// Message from a grabbed device's reader task
enum DeviceMessage {
    /// A raw event arrived from the given device
    Event(evdev::InputEvent, PathBuf),
    /// The device stopped producing events and is no longer held
    Gone(PathBuf),
}

impl Default for Platform {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformInterface for Platform {
    fn new() -> Self {
        Self {
            window_state: StdArc::new(WindowState::default()),
            dbus_conn: StdArc::new(OnceCell::new()),
            uinput_device: None,
            mpris_tracker: StdArc::new(Mutex::new(MprisPlayerTracker::new())),
        }
    }

    async fn run<F, Fut>(
        &mut self,
        bound_keys: &HashSet<KeyCode>,
        intercept_scroll: bool,
        mut handler: F,
    ) -> Result<()>
    where
        F: FnMut(InputEvent, PlatformHandle) -> Fut,
        Fut: Future<Output = EventResponse>,
    {
        info!("starting Linux input handler");

        check_permissions()?;
        report_display_environment();
        setup_panic_hook();

        let wanted: HashSet<evdev::KeyCode> = bound_keys
            .iter()
            .filter_map(|key| u16::try_from(key.code()).ok())
            .map(evdev::KeyCode::new)
            .collect();

        if wanted.is_empty() {
            warn!("no keys are bound; no devices will be grabbed");
        }

        // Holding a sender for the loop's lifetime keeps the channel open, so
        // the select branch stays live even while no devices are held.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<DeviceMessage>();

        let mut held: HashMap<PathBuf, DeviceRole> = HashMap::new();
        let mut mirrors: HashMap<PathBuf, VirtualDevice> = HashMap::new();
        let mut mirror_nodes: HashSet<PathBuf> = HashSet::new();
        for (path, role) in find_bindable_devices(&wanted, intercept_scroll, &mirror_nodes)? {
            grab_and_spawn(
                &path,
                role,
                &mut held,
                &mut mirrors,
                &mut mirror_nodes,
                &event_tx,
            );
        }

        if held.is_empty() {
            error!(
                "no input devices could be grabbed; waiting for one to appear. \
                 If another remapper holds them, stop it and rebinded will pick them up."
            );
        } else {
            info!("grabbed {} device(s)", held.len());
        }

        let uinput = create_virtual_keyboard()?;
        self.uinput_device = Some(StdArc::new(StdMutex::new(uinput)));
        info!("created virtual keyboard for re-injection");

        // Hotplug: inotify tells us when a node appears or becomes readable.
        let (hotplug_tx, mut hotplug_rx) = mpsc::unbounded_channel::<PathBuf>();
        spawn_device_watcher(hotplug_tx);

        // X11 thread: publishes window info for window conditions.
        let (window_tx, window_rx) = watch::channel(WindowInfo::default());
        spawn_x11_thread(StdArc::clone(&self.window_state), window_tx);

        tokio::spawn(mpris_focus_monitor(
            window_rx,
            StdArc::clone(&self.mpris_tracker),
            StdArc::clone(&self.dbus_conn),
        ));

        let platform_handle = PlatformHandle::new(self);
        let mut rescan = tokio::time::interval(DEVICE_RESCAN_INTERVAL);
        rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        rescan.tick().await; // the first tick resolves immediately

        // Events of a claimed pointer, held until SYN_REPORT closes the frame.
        let mut frames: HashMap<PathBuf, Vec<evdev::InputEvent>> = HashMap::new();
        // A detent decides the micro-steps that trail it, which arrive in
        // frames of their own once the wheel reports high resolution.
        let mut wheel_passthrough = true;

        loop {
            tokio::select! {
                Some(message) = event_rx.recv() => {
                    match message {
                        DeviceMessage::Gone(path) => {
                            held.remove(&path);
                            mirrors.remove(&path);
                            frames.remove(&path);
                            if held.is_empty() {
                                warn!(
                                    "last device released ({}); \
                                     waiting for a device to return",
                                    path.display()
                                );
                            } else {
                                info!("released device: {}", path.display());
                            }
                        }
                        DeviceMessage::Event(raw_event, path) => {
                            if held.get(&path) != Some(&DeviceRole::Pointer) {
                                if raw_event.event_type() != EventType::KEY {
                                    continue;
                                }
                                let Some(input_event) = convert_event(&raw_event) else {
                                    continue;
                                };

                                trace!(?input_event, "processing keyboard event");
                                let response = handler(input_event, platform_handle).await;

                                if response == EventResponse::Passthrough
                                    && let Some(ref uinput) = self.uinput_device
                                {
                                    let mut device = uinput
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    if let Err(e) = device.emit(&[raw_event]) {
                                        warn!("failed to emit passthrough event: {}", e);
                                    }
                                }
                                continue;
                            }

                            // Mirroring a frame at a time keeps multi-axis
                            // motion in one report, as the device sent it.
                            if raw_event.event_type() != EventType::SYNCHRONIZATION {
                                frames.entry(path).or_default().push(raw_event);
                                continue;
                            }

                            let Some(frame) = frames.remove(&path) else {
                                continue;
                            };

                            if let Some(up) = frame.iter().find_map(detent_direction) {
                                let input_event = InputEvent::Scroll { up };
                                trace!(?input_event, "processing scroll event from evdev");
                                wheel_passthrough = handler(input_event, platform_handle).await
                                    == EventResponse::Passthrough;
                            }

                            let mut out = Vec::with_capacity(frame.len());
                            for event in frame {
                                if is_vertical_wheel(&event) {
                                    if wheel_passthrough {
                                        out.push(event);
                                    }
                                    continue;
                                }

                                if event.event_type() == EventType::KEY
                                    && let Some(input_event) = convert_event(&event)
                                    && handler(input_event, platform_handle).await
                                        == EventResponse::Block
                                {
                                    continue;
                                }

                                out.push(event);
                            }

                            if !out.is_empty()
                                && let Some(mirror) = mirrors.get_mut(&path)
                                && let Err(e) = mirror.emit(&out)
                            {
                                warn!("failed to mirror pointer frame: {}", e);
                            }
                        }
                    }
                }

                Some(path) = hotplug_rx.recv() => {
                    if !held.contains_key(&path)
                        && let Some(role) =
                            should_grab_device(&path, &wanted, intercept_scroll, &mirror_nodes)
                    {
                        grab_and_spawn(
                            &path,
                            role,
                            &mut held,
                            &mut mirrors,
                            &mut mirror_nodes,
                            &event_tx,
                        );
                    }
                }

                _ = rescan.tick() => {
                    if let Ok(found) =
                        find_bindable_devices(&wanted, intercept_scroll, &mirror_nodes)
                    {
                        for (path, role) in found {
                            if !held.contains_key(&path) {
                                grab_and_spawn(
                                    &path,
                                    role,
                                    &mut held,
                                    &mut mirrors,
                                    &mut mirror_nodes,
                                    &event_tx,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn get_active_window(&self) -> WindowInfo {
        self.window_state.get()
    }

    fn send_key(&self, key: SyntheticKey) {
        let uinput = match &self.uinput_device {
            Some(device) => StdArc::clone(device),
            None => {
                warn!("uinput device not initialized");
                return;
            }
        };

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
            let mut device = uinput
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(e) = device.emit(&events) {
                warn!("failed to emit synthetic key: {}", e);
            } else {
                debug!(?key, "emitted synthetic key");
            }
        });
    }

    fn send_media(&self, cmd: MediaCommand) {
        let dbus_conn = StdArc::clone(&self.dbus_conn);
        let tracker = StdArc::clone(&self.mpris_tracker);
        let window_info = self.get_active_window();

        tokio::spawn(async move {
            match cmd {
                MediaCommand::VolumeUp | MediaCommand::VolumeDown | MediaCommand::VolumeMute => {
                    send_volume_command(cmd).await;
                    return;
                }
                _ => {}
            }

            if let Err(e) = send_mpris_command(&dbus_conn, cmd, &window_info, tracker).await {
                warn!("media command {:?} failed: {}", cmd, e);
            }
        });
    }
}

/// Log what display server the process can actually reach.
///
/// The environment is fixed at exec, so a missing DISPLAY is permanent.
fn report_display_environment() {
    let x11 = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty());
    let wayland = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty());

    match (&x11, &wayland) {
        (Some(x11), _) => info!("X11 display {}", x11),
        (None, Some(wayland)) => warn!(
            "running under Wayland ({}) with no DISPLAY; window conditions \
             require X11 or XWayland and will be unavailable",
            wayland
        ),
        (None, None) => warn!(
            "DISPLAY is not set; window conditions will be unavailable. Under \
             systemd, the unit must be WantedBy=graphical-session.target so it \
             starts after the session exports DISPLAY."
        ),
    }
}

/// Atoms interned once per connection.
struct Atoms {
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    utf8_string: u32,
    window: u32,
    cardinal: u32,
}

impl Atoms {
    fn intern(conn: &x11rb::rust_connection::RustConnection) -> Result<Self> {
        use x11rb::protocol::xproto::ConnectionExt as _;

        // Issue every request before collecting replies so this costs one round
        // trip rather than six.
        let net_active_window = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?;
        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?;
        let net_wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?;
        let window = conn.intern_atom(false, b"WINDOW")?;
        let cardinal = conn.intern_atom(false, b"CARDINAL")?;

        Ok(Self {
            net_active_window: net_active_window.reply()?.atom,
            net_wm_name: net_wm_name.reply()?.atom,
            net_wm_pid: net_wm_pid.reply()?.atom,
            utf8_string: utf8_string.reply()?.atom,
            window: window.reply()?.atom,
            cardinal: cardinal.reply()?.atom,
        })
    }
}

/// Spawn the thread that owns the X11 connection.
///
/// Reconnects with backoff, so a display that is not up yet resolves itself.
fn spawn_x11_thread(window_state: StdArc<WindowState>, window_tx: watch::Sender<WindowInfo>) {
    let spawned = std::thread::Builder::new()
        .name("rebinded-x11".to_string())
        .spawn(move || {
            let mut backoff = X11_RECONNECT_MIN;
            let mut announced_failure = false;

            loop {
                match x11_session(&window_state, &window_tx) {
                    Ok(()) => {
                        debug!("X11 thread stopping; event channel closed");
                        return;
                    }
                    Err(e) => {
                        // Announce the first failure, then stay quiet: a display
                        // that is not up yet should not spam the journal.
                        if announced_failure {
                            debug!("X11 session ended: {}", e);
                        } else {
                            warn!(
                                "X11 unavailable: {}. Window conditions are disabled \
                                 until it returns; retrying in the background.",
                                e
                            );
                            announced_failure = true;
                        }
                    }
                }

                // Stale window info is worse than none: a condition matching a
                // window that no longer has focus fires the wrong action.
                window_state.set(WindowInfo::default());
                let _ = window_tx.send(WindowInfo::default());

                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(X11_RECONNECT_MAX);
            }
        });

    if let Err(e) = spawned {
        error!(
            "failed to spawn X11 thread: {}. Window conditions will be unavailable.",
            e
        );
    }
}

/// One connected X11 session: set up, then serve events until the connection drops.
fn x11_session(
    window_state: &StdArc<WindowState>,
    window_tx: &watch::Sender<WindowInfo>,
) -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask};

    let (conn, screen_num) = x11rb::connect(None).context("failed to connect")?;
    let root = conn.setup().roots[screen_num].root;
    let atoms = Atoms::intern(&conn).context("failed to intern atoms")?;

    // Watch the root for focus changes.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?
    .check()
    .context("failed to select property events on root")?;

    info!("X11 connected; window conditions active");

    // Publish once up front so bindings work before the first focus change.
    let mut tracked = publish_window(&conn, &atoms, root, None, window_state, window_tx);

    loop {
        let event = conn.wait_for_event().context("connection lost")?;

        match event {
            // Browsers retitle on tab switch without changing
            // _NET_ACTIVE_WINDOW, so the window is watched as well as root.
            Event::PropertyNotify(ev)
                if (ev.window == root && ev.atom == atoms.net_active_window)
                    || (Some(ev.window) == tracked
                        && (ev.atom == atoms.net_wm_name
                            || ev.atom
                                == u32::from(x11rb::protocol::xproto::AtomEnum::WM_NAME))) =>
            {
                tracked = publish_window(&conn, &atoms, root, tracked, window_state, window_tx);
            }

            _ => {}
        }
    }
}

/// Re-read the focused window and publish it if it changed.
///
/// Returns the window now being tracked for title changes.
fn publish_window(
    conn: &x11rb::rust_connection::RustConnection,
    atoms: &Atoms,
    root: u32,
    tracked: Option<u32>,
    window_state: &StdArc<WindowState>,
    window_tx: &watch::Sender<WindowInfo>,
) -> Option<u32> {
    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask};

    let active = match active_window_id(conn, atoms, root) {
        Ok(id) => id,
        Err(e) => {
            debug!("failed to read active window: {}", e);
            return tracked;
        }
    };

    if active != tracked {
        // Stop listening to the window we are leaving; it may already be gone,
        // so errors here are expected and ignored.
        if let Some(previous) = tracked {
            conn.change_window_attributes(
                previous,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            )
            .map(|cookie| cookie.ignore_error())
            .ok();
        }
        if let Some(current) = active {
            conn.change_window_attributes(
                current,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map(|cookie| cookie.ignore_error())
            .ok();
        }
    }

    let info = match active {
        Some(window) => query_window_info(conn, atoms, window),
        None => WindowInfo::default(),
    };

    if window_state.get() != info {
        trace!(?info, "focused window changed");
        window_state.set(info.clone());
        let _ = window_tx.send(info);
    }

    active
}

/// Read `_NET_ACTIVE_WINDOW` from the root window.
fn active_window_id(
    conn: &x11rb::rust_connection::RustConnection,
    atoms: &Atoms,
    root: u32,
) -> Result<Option<u32>> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let reply = conn
        .get_property(false, root, atoms.net_active_window, atoms.window, 0, 1)?
        .reply()?;

    if reply.value.len() < 4 {
        return Ok(None);
    }

    let id = u32::from_ne_bytes([
        reply.value[0],
        reply.value[1],
        reply.value[2],
        reply.value[3],
    ]);

    // Some window managers publish 0 to mean "nothing focused".
    Ok((id != 0).then_some(id))
}

/// Collect title, class and binary for a window. Missing pieces are left empty.
fn query_window_info(
    conn: &x11rb::rust_connection::RustConnection,
    atoms: &Atoms,
    window: u32,
) -> WindowInfo {
    WindowInfo {
        title: window_title(conn, atoms, window).unwrap_or_default(),
        class: window_class(conn, window).unwrap_or_default(),
        binary: window_binary(conn, atoms, window).unwrap_or_default(),
    }
}

/// Get window title, preferring the UTF-8 `_NET_WM_NAME` over legacy `WM_NAME`.
fn window_title(
    conn: &x11rb::rust_connection::RustConnection,
    atoms: &Atoms,
    window: u32,
) -> Result<String> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let reply = conn
        .get_property(false, window, atoms.net_wm_name, atoms.utf8_string, 0, 1024)?
        .reply()?;

    if !reply.value.is_empty()
        && let Ok(title) = String::from_utf8(reply.value)
    {
        return Ok(title);
    }

    let reply = conn
        .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?
        .reply()?;

    Ok(String::from_utf8_lossy(&reply.value).into_owned())
}

/// Get window class (second element of `WM_CLASS`)
fn window_class(conn: &x11rb::rust_connection::RustConnection, window: u32) -> Result<String> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

    let reply = conn
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)?
        .reply()?;

    // WM_CLASS format: "instance\0class\0"
    let value = String::from_utf8_lossy(&reply.value);
    Ok(value.split('\0').nth(1).unwrap_or("").to_string())
}

/// Get window binary name by resolving `_NET_WM_PID` through /proc
fn window_binary(
    conn: &x11rb::rust_connection::RustConnection,
    atoms: &Atoms,
    window: u32,
) -> Result<String> {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let reply = conn
        .get_property(false, window, atoms.net_wm_pid, atoms.cardinal, 0, 1)?
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

    let exe = std::fs::read_link(format!("/proc/{}/exe", pid))?;
    Ok(exe
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default())
}

/// What, if anything, a device should be taken over exclusively for.
///
/// A wheel device is claimed only when a binding actually diverts scrolling.
/// The claim covers the whole device, so its motion is mirrored back out; that
/// is why the mirror copies the identity libinput accelerates from.
fn should_grab_device(
    path: &Path,
    wanted: &HashSet<evdev::KeyCode>,
    want_scroll: bool,
    mirrors: &HashSet<PathBuf>,
) -> Option<DeviceRole> {
    // A mirror carries its source's name, so it is only told apart by node.
    if mirrors.contains(path) {
        return None;
    }

    let device = Device::open(path).ok()?;

    if device.name().unwrap_or_default() == VIRTUAL_KEYBOARD_NAME {
        return None;
    }

    // Injectors like ydotoold carry a wheel but report no physical path.
    // Claiming one would swallow the events another tool is synthesizing.
    let is_physical = device.physical_path().is_some_and(|phys| !phys.is_empty());

    let axes = device.supported_relative_axes();
    let has_wheel = axes.is_some_and(|axes| axes.contains(RelativeAxisCode::REL_WHEEL));
    if want_scroll && has_wheel && is_physical {
        return Some(DeviceRole::Pointer);
    }

    // Without a wheel to intercept, re-injecting REL_X/REL_Y would only cost
    // the pointer feel, so motion devices are left alone.
    if axes.is_some_and(|axes| axes.contains(RelativeAxisCode::REL_X)) {
        return None;
    }

    let produces_bound_key = device
        .supported_keys()
        .is_some_and(|keys| wanted.iter().any(|key| keys.contains(*key)));

    produces_bound_key.then_some(DeviceRole::Keyboard)
}

/// Find every device worth claiming, with the role to claim it for.
fn find_bindable_devices(
    wanted: &HashSet<evdev::KeyCode>,
    want_scroll: bool,
    mirrors: &HashSet<PathBuf>,
) -> Result<Vec<(PathBuf, DeviceRole)>> {
    let mut devices = Vec::new();

    for entry in std::fs::read_dir("/dev/input").context("failed to read /dev/input directory")? {
        let path = entry?.path();

        let is_event_node = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"));

        if !is_event_node {
            continue;
        }

        if let Some(role) = should_grab_device(&path, wanted, want_scroll, mirrors) {
            devices.push((path, role));
        }
    }

    Ok(devices)
}

/// Build a uinput device that mirrors `source`, and the nodes it appears at.
///
/// The name, ids and phys are copied verbatim because udev's hwdb keys the
/// mouse DPI off them. A mirror named anything else misses the match, and
/// libinput then accelerates the pointer against a default DPI instead of the
/// real one. Since that makes the mirror indistinguishable by name, its device
/// nodes are returned so we can avoid claiming our own output.
fn create_device_mirror(source: &Device) -> Result<(VirtualDevice, Vec<PathBuf>)> {
    // The hwdb key is built from the bus, ids and name, so those are what the
    // DPI lookup needs. Phys is left unset: UI_SET_PHYS rejects it here.
    let mut builder = VirtualDevice::builder()?
        .name(source.name().unwrap_or("rebinded-mirror"))
        .input_id(source.input_id())
        .with_properties(source.properties())?;

    if let Some(keys) = source.supported_keys() {
        builder = builder.with_keys(keys)?;
    }
    if let Some(axes) = source.supported_relative_axes() {
        builder = builder.with_relative_axes(axes)?;
    }
    if let Some(misc) = source.misc_properties() {
        builder = builder.with_msc(misc)?;
    }

    let mut device = builder.build().context("failed to create mirror")?;
    let nodes = device
        .enumerate_dev_nodes_blocking()
        .context("failed to resolve mirror nodes")?
        .filter_map(Result::ok)
        .collect();

    Ok((device, nodes))
}

/// Grab a device and spawn its reader task, recording it as held on success.
///
/// A pointer claim also builds the mirror its events are replayed through; if
/// that fails the claim is dropped, since holding it would silence the device.
fn grab_and_spawn(
    path: &Path,
    role: DeviceRole,
    held: &mut HashMap<PathBuf, DeviceRole>,
    mirrors: &mut HashMap<PathBuf, VirtualDevice>,
    mirror_nodes: &mut HashSet<PathBuf>,
    event_tx: &mpsc::UnboundedSender<DeviceMessage>,
) {
    let mut device = match Device::open(path) {
        Ok(device) => device,
        Err(e) => {
            debug!("failed to open {}: {}", path.display(), e);
            return;
        }
    };

    let name = device.name().unwrap_or("unknown").to_string();

    let mirror = if role == DeviceRole::Pointer {
        match create_device_mirror(&device) {
            Ok((mirror, nodes)) => {
                mirror_nodes.extend(nodes);
                Some(mirror)
            }
            Err(e) => {
                warn!("no mirror for {}, leaving it alone: {:#}", name, e);
                return;
            }
        }
    } else {
        None
    };

    if let Err(e) = device.grab() {
        // Contention is normal and often transient (another remapper, or a
        // node that is not settled yet), so the periodic re-scan retries.
        debug!("failed to grab {} ({}): {}", name, path.display(), e);
        return;
    }

    info!(
        "grabbed device: {} ({}) as {:?}",
        name,
        path.display(),
        role
    );
    held.insert(path.to_path_buf(), role);
    if let Some(mirror) = mirror {
        mirrors.insert(path.to_path_buf(), mirror);
    }

    let path = path.to_path_buf();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = process_device_events(device, path.clone(), &tx).await {
            debug!("device {} stopped: {}", path.display(), e);
        }
        let _ = tx.send(DeviceMessage::Gone(path));
    });
}

/// Watch `/dev/input` for devices appearing or becoming accessible.
///
/// IN_ATTRIB matters as much as IN_CREATE: udev sets permissions after create.
fn spawn_device_watcher(hotplug_tx: mpsc::UnboundedSender<PathBuf>) {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};

    let spawned = std::thread::Builder::new()
        .name("rebinded-hotplug".to_string())
        .spawn(move || {
            let inotify = match Inotify::init(InitFlags::empty()) {
                Ok(inotify) => inotify,
                Err(e) => {
                    warn!(
                        "failed to init inotify ({}); relying on periodic re-scan",
                        e
                    );
                    return;
                }
            };

            if let Err(e) = inotify.add_watch(
                "/dev/input",
                AddWatchFlags::IN_CREATE | AddWatchFlags::IN_ATTRIB,
            ) {
                warn!(
                    "failed to watch /dev/input ({}); relying on periodic re-scan",
                    e
                );
                return;
            }

            debug!("watching /dev/input for hotplug events");

            loop {
                let events = match inotify.read_events() {
                    Ok(events) => events,
                    Err(e) => {
                        warn!("inotify read failed: {}; hotplug detection stopped", e);
                        return;
                    }
                };

                for event in events {
                    let Some(name) = event.name else { continue };
                    let Some(name) = name.to_str() else { continue };
                    if !name.starts_with("event") {
                        continue;
                    }

                    let path = PathBuf::from("/dev/input").join(name);
                    trace!("hotplug event for {}", path.display());
                    if hotplug_tx.send(path).is_err() {
                        return;
                    }
                }
            }
        });

    if let Err(e) = spawned {
        warn!(
            "failed to spawn hotplug watcher: {}; relying on periodic re-scan",
            e
        );
    }
}

/// Process events from a single device
async fn process_device_events(
    device: Device,
    device_path: PathBuf,
    event_tx: &mpsc::UnboundedSender<DeviceMessage>,
) -> Result<()> {
    let mut stream = device.into_event_stream()?;

    loop {
        let event = stream.next_event().await?;
        if event_tx
            .send(DeviceMessage::Event(event, device_path.clone()))
            .is_err()
        {
            return Ok(());
        }
    }
}

/// Convert evdev InputEvent to our InputEvent type
fn convert_event(ev: &evdev::InputEvent) -> Option<InputEvent> {
    match ev.event_type() {
        EventType::KEY => {
            // value: 1 = press, 0 = release, 2 = auto-repeat
            if ev.value() == 2 {
                return None;
            }
            Some(InputEvent::Key(KeyEvent::new(
                KeyCode::new(ev.code() as u32),
                ev.value() == 1,
            )))
        }
        EventType::RELATIVE => {
            // REL_WHEEL: value > 0 = up (away from user), value < 0 = down
            if ev.code() == RelativeAxisCode::REL_WHEEL.0 {
                Some(InputEvent::Scroll { up: ev.value() > 0 })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether an event carries vertical wheel movement.
///
/// The high resolution axis is included so a blocked tick does not leak through
/// as smooth scrolling, which is the axis modern toolkits actually read.
fn is_vertical_wheel(ev: &evdev::InputEvent) -> bool {
    ev.event_type() == EventType::RELATIVE
        && (ev.code() == RelativeAxisCode::REL_WHEEL.0
            || ev.code() == RelativeAxisCode::REL_WHEEL_HI_RES.0)
}

/// The direction of a wheel detent, if this event is one.
fn detent_direction(ev: &evdev::InputEvent) -> Option<bool> {
    let is_detent = ev.event_type() == EventType::RELATIVE
        && ev.code() == RelativeAxisCode::REL_WHEEL.0
        && ev.value() != 0;

    is_detent.then(|| ev.value() > 0)
}

/// Create a virtual keyboard for re-injecting events
///
/// Keys only: relative axes make udev tag the device a mouse, which splits it
/// into a pointer plus keyboard subdevice under libinput. A claimed pointer is
/// replayed through its own mirror instead.
fn create_virtual_keyboard() -> Result<VirtualDevice> {
    use evdev::AttributeSet;

    let mut keys = AttributeSet::<evdev::KeyCode>::new();
    for code in 0..=767u16 {
        keys.insert(evdev::KeyCode::new(code));
    }

    let device = VirtualDevice::builder()?
        .name(VIRTUAL_KEYBOARD_NAME)
        .with_keys(&keys)?
        .build()?;

    Ok(device)
}

/// Create a SYN_REPORT synchronization event
fn create_syn_report() -> evdev::InputEvent {
    evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION.0, 0, 0)
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
/// More reliable than XF86Audio keys, which not every desktop picks up.
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

/// Get the shared session bus connection, establishing it on first use.
async fn session_bus(cell: &OnceCell<zbus::Connection>) -> Result<&zbus::Connection> {
    cell.get_or_try_init(|| async { zbus::Connection::session().await })
        .await
        .context("failed to connect to the session bus")
}

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
        name.split('.').next().unwrap_or(name)
    }

    /// Check if this player matches the given window info
    /// Matches against window binary name and class (case-insensitive)
    fn matches_window(&self, window: &WindowInfo) -> bool {
        let player_name = self.player_name().to_lowercase();
        let identity = self.identity.to_lowercase();

        let binary_name = window
            .binary
            .rsplit('/')
            .next()
            .unwrap_or(&window.binary)
            .to_lowercase();
        let class = window.class.to_lowercase();

        let matches = |a: &str, b: &str| -> bool {
            !a.is_empty() && !b.is_empty() && (a.contains(b) || b.contains(a))
        };

        matches(&binary_name, &player_name)
            || matches(&class, &player_name)
            || matches(&binary_name, &identity)
            || matches(&class, &identity)
    }

    /// Check if this player shares a process family with the window
    ///
    /// Looser than `matches_window`: "vivaldi" matches "vivaldi-bin".
    fn matches_process_family(&self, window: &WindowInfo) -> bool {
        let player_name = self.player_name().to_lowercase();

        let binary_name = window
            .binary
            .rsplit('/')
            .next()
            .unwrap_or(&window.binary)
            .to_lowercase();

        if binary_name.is_empty() {
            return false;
        }

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

        binary_base == player_base
            || binary_base.starts_with(player_base)
            || player_base.starts_with(binary_base)
    }
}

/// Tracks historical state for MPRIS player selection
///
/// Remembering recent focus and playback lets media keys reach the intended
/// player even when an unrelated window is focused.
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
        for player_name in &self.known_players {
            let candidate = MprisPlayerInfo {
                service_name: format!("org.mpris.MediaPlayer2.{}", player_name),
                identity: player_name.clone(),
                playback_status: String::new(),
            };
            if candidate.matches_window(window) || candidate.matches_process_family(window) {
                return Some(player_name);
            }
        }
        None
    }
}

/// Records which media player windows get focused, for player selection.
///
/// Driven by focus updates rather than a timer, so an idle desktop costs nothing.
async fn mpris_focus_monitor(
    mut window_rx: watch::Receiver<WindowInfo>,
    tracker: StdArc<Mutex<MprisPlayerTracker>>,
    dbus_cell: StdArc<OnceCell<zbus::Connection>>,
) {
    let mut last_focused_player: Option<String> = None;

    while window_rx.changed().await.is_ok() {
        let window_info = window_rx.borrow_and_update().clone();
        if window_info.binary.is_empty() && window_info.class.is_empty() {
            last_focused_player = None;
            continue;
        }

        let Ok(conn) = session_bus(&dbus_cell).await else {
            continue;
        };

        // Refresh the player list outside the tracker lock so a slow bus call
        // cannot stall a media keypress waiting on the same lock.
        let refresh_needed = tracker.lock().await.needs_player_refresh();
        if refresh_needed && let Some(services) = list_mpris_players(conn).await {
            let names = services
                .iter()
                .filter_map(|service| {
                    service
                        .strip_prefix("org.mpris.MediaPlayer2.")
                        .map(|name| name.split('.').next().unwrap_or(name).to_string())
                })
                .collect();
            tracker.lock().await.update_known_players(names);
        }

        let mut guard = tracker.lock().await;
        match guard.find_matching_player(&window_info) {
            Some(player_name) => {
                let player_name = player_name.to_string();
                if last_focused_player.as_ref() != Some(&player_name) {
                    debug!(
                        "focus changed to player: {} (window: {})",
                        player_name, window_info.class
                    );
                    guard.record_focus(&player_name);
                    last_focused_player = Some(player_name);
                }
            }
            None => last_focused_player = None,
        }
    }
}

/// Send MPRIS media command with smart player selection
async fn send_mpris_command(
    dbus_cell: &OnceCell<zbus::Connection>,
    cmd: MediaCommand,
    window_info: &WindowInfo,
    tracker: StdArc<Mutex<MprisPlayerTracker>>,
) -> Result<()> {
    use zbus::proxy;

    let conn = session_bus(dbus_cell).await?;

    let player_name = find_best_mpris_player(conn, window_info, &tracker)
        .await
        .context("no MPRIS media players found")?;

    debug!("sending MPRIS command {:?} to {}", cmd, player_name);

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

    let proxy = MediaPlayer2PlayerProxy::builder(conn)
        .destination(player_name)?
        .build()
        .await?;

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
    let player_services = list_mpris_players(conn).await?;

    let mut players: Vec<MprisPlayerInfo> = Vec::new();
    for service in player_services {
        if let Some(info) = get_mpris_player_info(conn, &service).await {
            players.push(info);
        }
    }

    if players.is_empty() {
        return None;
    }

    let mut guard = tracker.lock().await;
    for player in &players {
        if player.is_playing() {
            guard.record_playing(player.player_name());
        }
    }

    debug!(
        "found {} MPRIS players, focused window: binary={:?} class={:?}",
        players.len(),
        window_info.binary,
        window_info.class
    );

    for player in &players {
        let player_name = player.player_name();
        debug!(
            "player {} (identity={}, playing={}, window_match={}, family_match={}, last_focus={:?}, last_playing={:?})",
            player.service_name,
            player.identity,
            player.is_playing(),
            player.matches_window(window_info),
            player.matches_process_family(window_info),
            guard.get_valid_focus(player_name).map(|t| t.elapsed()),
            guard.get_last_playing(player_name).map(|t| t.elapsed()),
        );
    }

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
                guard
                    .get_valid_focus(a_name)
                    .cmp(&guard.get_valid_focus(b_name))
            })
            .then_with(|| {
                guard
                    .get_last_playing(a_name)
                    .cmp(&guard.get_last_playing(b_name))
            })
    });

    best_player.map(|player| {
        debug!("selected player: {}", player.service_name);
        player.service_name.clone()
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

    let identity = proxy
        .get("org.mpris.MediaPlayer2", "Identity")
        .await
        .ok()
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_default();

    let playback_status = proxy
        .get("org.mpris.MediaPlayer2.Player", "PlaybackStatus")
        .await
        .ok()
        .and_then(|value| String::try_from(value).ok())
        .unwrap_or_else(|| "Stopped".to_string());

    Some(MprisPlayerInfo {
        service_name: service.to_string(),
        identity,
        playback_status,
    })
}

/// Check system permissions and requirements
fn check_permissions() -> Result<()> {
    if !Path::new("/dev/input").exists() {
        return Err(anyhow!("/dev/input not found. Are you running on Linux?"));
    }

    let readable = std::fs::read_dir("/dev/input")?
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            let path = entry.path();
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("event"))
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

    if !Path::new("/dev/uinput").exists() {
        return Err(anyhow!(
            "/dev/uinput not found. Load the uinput module:\n  \
            sudo modprobe uinput\n\n\
            To load automatically at boot:\n  \
            echo uinput | sudo tee /etc/modules-load.d/uinput.conf"
        ));
    }

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
        warn!("panic detected, attempting to ungrab devices");
        let _ = ungrab_all_devices();
        default_hook(panic_info);
    }));
}

/// Attempt to ungrab all devices (best effort)
fn ungrab_all_devices() -> Result<()> {
    for entry in std::fs::read_dir("/dev/input")? {
        let path = entry?.path();
        if let Some(filename) = path.file_name().and_then(|name| name.to_str())
            && filename.starts_with("event")
            && let Ok(mut device) = Device::open(&path)
        {
            let _ = device.ungrab();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn rel(code: RelativeAxisCode, value: i32) -> evdev::InputEvent {
        evdev::InputEvent::new(EventType::RELATIVE.0, code.0, value)
    }

    #[test]
    fn detent_reports_its_direction() {
        check!(detent_direction(&rel(RelativeAxisCode::REL_WHEEL, 1)) == Some(true));
        check!(detent_direction(&rel(RelativeAxisCode::REL_WHEEL, -1)) == Some(false));
    }

    #[test]
    fn only_a_detent_decides_a_tick() {
        // Micro-steps trail a detent; treating each as its own tick would run
        // the divert action several times for one notch of the wheel.
        check!(detent_direction(&rel(RelativeAxisCode::REL_WHEEL_HI_RES, 120)).is_none());
        check!(detent_direction(&rel(RelativeAxisCode::REL_WHEEL, 0)).is_none());
        check!(detent_direction(&rel(RelativeAxisCode::REL_X, 5)).is_none());
    }

    #[test]
    fn both_vertical_wheel_axes_are_suppressed_together() {
        // Passing the high resolution axis through would let a blocked tick
        // still scroll, since that is the axis toolkits read.
        check!(is_vertical_wheel(&rel(RelativeAxisCode::REL_WHEEL, 1)));
        check!(is_vertical_wheel(&rel(
            RelativeAxisCode::REL_WHEEL_HI_RES,
            120
        )));
    }

    #[test]
    fn motion_and_horizontal_scroll_are_left_alone() {
        check!(!is_vertical_wheel(&rel(RelativeAxisCode::REL_X, 5)));
        check!(!is_vertical_wheel(&rel(RelativeAxisCode::REL_Y, -3)));
        check!(!is_vertical_wheel(&rel(RelativeAxisCode::REL_HWHEEL, 1)));
        check!(!is_vertical_wheel(&evdev::InputEvent::new(
            EventType::KEY.0,
            evdev::KeyCode::BTN_LEFT.0,
            1
        )));
    }
}
