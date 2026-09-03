//! Linux-specific platform implementation
//!
//! Key components:
//! - evdev for raw input device access and virtual device creation
//! - X11 (via x11rb) for window queries and scroll wheel interception
//! - D-Bus (via zbus) for MPRIS media control
//!
//! One dedicated thread owns the single X11 connection and both publishes the
//! focused window and runs the scroll grab, so the two recover together and no
//! blocking round trip lands on the tokio runtime that processes key events.
//! `get_active_window` reads a cached snapshot.

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

/// How long the pointer may stay frozen deciding one scroll tick.
///
/// The grab freezes motion too, so exceeding this is felt as stutter.
const POINTER_FREEZE_BUDGET: Duration = Duration::from_millis(4);

/// Minimum gap between freeze warnings, so a slow run reports without flooding.
const FREEZE_WARNING_INTERVAL: Duration = Duration::from_secs(10);

/// X11 pointer names whose scroll events the grab must ignore.
///
/// Skipping our own device is what stops a replayed tick being recaptured.
const SYNTHETIC_POINTER_NAMES: [&str; 4] = ["rebinded", "XTEST", "Virtual core", "ydotoold"];

/// Name of the uinput device we re-inject through, and the name we refuse to
/// grab so our own output cannot feed back in.
const VIRTUAL_KEYBOARD_NAME: &str = "rebinded-virtual-keyboard";

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

    async fn run<F, Fut>(&mut self, bound_keys: &HashSet<KeyCode>, mut handler: F) -> Result<()>
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

        let mut held: HashSet<PathBuf> = HashSet::new();
        for path in find_bindable_devices(&wanted)? {
            grab_and_spawn(&path, &mut held, &event_tx);
        }

        if held.is_empty() {
            error!(
                "no keyboard devices could be grabbed; waiting for one to appear. \
                 If another remapper holds them, stop it and rebinded will pick them up."
            );
        } else {
            info!("grabbed {} keyboard device(s)", held.len());
        }

        let uinput = create_virtual_keyboard()?;
        self.uinput_device = Some(StdArc::new(StdMutex::new(uinput)));
        info!("created virtual keyboard for re-injection");

        // Hotplug: inotify tells us when a node appears or becomes readable.
        let (hotplug_tx, mut hotplug_rx) = mpsc::unbounded_channel::<PathBuf>();
        spawn_device_watcher(hotplug_tx);

        // X11 thread: publishes window info and drives the scroll grab.
        let (scroll_tx, mut scroll_rx) = mpsc::unbounded_channel::<bool>();
        let (replay_tx, replay_rx) = crossbeam_channel::unbounded::<bool>();
        let (window_tx, window_rx) = watch::channel(WindowInfo::default());
        spawn_x11_thread(
            StdArc::clone(&self.window_state),
            scroll_tx,
            replay_rx,
            window_tx,
        );

        tokio::spawn(mpris_focus_monitor(
            window_rx,
            StdArc::clone(&self.mpris_tracker),
            StdArc::clone(&self.dbus_conn),
        ));

        let platform_handle = PlatformHandle::new(self);
        let mut rescan = tokio::time::interval(DEVICE_RESCAN_INTERVAL);
        rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        rescan.tick().await; // the first tick resolves immediately

        loop {
            tokio::select! {
                Some(message) = event_rx.recv() => {
                    match message {
                        DeviceMessage::Gone(path) => {
                            held.remove(&path);
                            if held.is_empty() {
                                warn!(
                                    "last keyboard device released ({}); \
                                     waiting for a device to return",
                                    path.display()
                                );
                            } else {
                                info!("released device: {}", path.display());
                            }
                        }
                        DeviceMessage::Event(raw_event, _path) => {
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
                        }
                    }
                }

                // Scroll ticks arrive frozen: the X11 thread is blocked until we
                // answer, so this branch must always reply on replay_tx.
                Some(scroll_up) = scroll_rx.recv() => {
                    let input_event = InputEvent::Scroll { up: scroll_up };
                    trace!(?input_event, "processing scroll event from XInput2");

                    let response = handler(input_event, platform_handle).await;
                    let should_replay = response == EventResponse::Passthrough;
                    if let Err(e) = replay_tx.send(should_replay) {
                        warn!("failed to send scroll replay decision: {}", e);
                    }
                }

                Some(path) = hotplug_rx.recv() => {
                    if !held.contains(&path) && should_grab_device(&path, &wanted) {
                        grab_and_spawn(&path, &mut held, &event_tx);
                    }
                }

                _ = rescan.tick() => {
                    if let Ok(paths) = find_bindable_devices(&wanted) {
                        for path in paths {
                            if !held.contains(&path) {
                                grab_and_spawn(&path, &mut held, &event_tx);
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
            "running under Wayland ({}) with no DISPLAY; window conditions and \
             scroll bindings require X11 or XWayland and will be unavailable",
            wayland
        ),
        (None, None) => warn!(
            "DISPLAY is not set; window conditions and scroll bindings will be \
             unavailable. Under systemd, the unit must be WantedBy=graphical-session.target \
             so it starts after the session exports DISPLAY."
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
fn spawn_x11_thread(
    window_state: StdArc<WindowState>,
    scroll_tx: mpsc::UnboundedSender<bool>,
    replay_rx: crossbeam_channel::Receiver<bool>,
    window_tx: watch::Sender<WindowInfo>,
) {
    let spawned = std::thread::Builder::new()
        .name("rebinded-x11".to_string())
        .spawn(move || {
            let mut backoff = X11_RECONNECT_MIN;
            let mut announced_failure = false;

            loop {
                match x11_session(&window_state, &scroll_tx, &replay_rx, &window_tx) {
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
                                "X11 unavailable: {}. Window conditions and scroll bindings \
                                 are disabled until it returns; retrying in the background.",
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
            "failed to spawn X11 thread: {}. Window conditions and scroll bindings \
             will be unavailable.",
            e
        );
    }
}

/// One connected X11 session: set up, then serve events until the connection drops.
fn x11_session(
    window_state: &StdArc<WindowState>,
    scroll_tx: &mpsc::UnboundedSender<bool>,
    replay_rx: &crossbeam_channel::Receiver<bool>,
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

    let scroll = ScrollGrab::establish(&conn, root)?;
    info!("X11 connected; window conditions active");
    if scroll.is_some() {
        info!("XInput2 scroll grab active; scroll wheel bindings enabled");
    }

    // Publish once up front so bindings work before the first focus change.
    let mut tracked = publish_window(&conn, &atoms, root, None, window_state, window_tx);
    let mut last_freeze_warning: Option<Instant> = None;

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

            Event::XinputButtonPress(ev) => {
                let Some(grab) = scroll.as_ref() else {
                    continue;
                };
                let Some(up) = grab.classify(&ev) else {
                    continue;
                };

                // The device is frozen until XIAllowEvents. If the handler is
                // gone we must still thaw it, or the pointer stays stuck.
                let frozen_since = Instant::now();
                let should_replay = if scroll_tx.send(up).is_ok() {
                    replay_rx.recv().unwrap_or(true)
                } else {
                    grab.allow(&conn, true);
                    return Ok(());
                };

                grab.allow(&conn, should_replay);

                let frozen = frozen_since.elapsed();
                if frozen > POINTER_FREEZE_BUDGET
                    && last_freeze_warning
                        .is_none_or(|last| last.elapsed() >= FREEZE_WARNING_INTERVAL)
                {
                    warn!(
                        "pointer frozen {:?} deciding one scroll tick (budget {:?}); \
                         mouse motion stalls this long on every tick while scrolling",
                        frozen, POINTER_FREEZE_BUDGET
                    );
                    last_freeze_warning = Some(Instant::now());
                } else {
                    trace!("scroll tick decided in {:?}", frozen);
                }
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

/// An active XInput2 passive grab on the scroll wheel buttons.
struct ScrollGrab {
    /// Master pointer the grab was established on
    pointer: u16,
    /// Root window the grab was established on
    root: u32,
    /// Slave pointers whose events we accept, keyed by XInput source id
    physical: HashSet<u16>,
}

impl ScrollGrab {
    /// Grab scroll up/down on the master pointer.
    ///
    /// `Ok(None)` means no usable XInput2; window conditions still work.
    fn establish(conn: &x11rb::rust_connection::RustConnection, root: u32) -> Result<Option<Self>> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xinput::{self, ConnectionExt as _, EventMask};
        use x11rb::protocol::xproto::GrabStatus;

        let version = conn
            .xinput_xi_query_version(2, 0)
            .map_err(anyhow::Error::from)
            .and_then(|cookie| cookie.reply().map_err(anyhow::Error::from));

        let version = match version {
            Ok(version) => version,
            Err(e) => {
                warn!(
                    "XInput2 unavailable ({}); scroll wheel bindings will not work",
                    e
                );
                return Ok(None);
            }
        };
        debug!(
            "XInput2 version {}.{}",
            version.major_version, version.minor_version
        );

        let devices = conn.xinput_xi_query_device(xinput::Device::ALL)?.reply()?;

        // Grab on the master pointer rather than assuming the conventional id 2.
        let Some(pointer) = devices
            .infos
            .iter()
            .find(|info| info.type_ == xinput::DeviceType::MASTER_POINTER)
            .map(|info| info.deviceid)
        else {
            warn!("no master pointer found; scroll wheel bindings will not work");
            return Ok(None);
        };

        let physical: HashSet<u16> = devices
            .infos
            .iter()
            .filter(|info| info.type_ == xinput::DeviceType::SLAVE_POINTER)
            .filter(|info| {
                let name = String::from_utf8_lossy(&info.name);
                !SYNTHETIC_POINTER_NAMES
                    .iter()
                    .any(|synthetic| name.contains(synthetic))
            })
            .map(|info| {
                debug!(
                    "accepting scroll from pointer {} (id={})",
                    String::from_utf8_lossy(&info.name).trim_end_matches('\0'),
                    info.deviceid
                );
                info.deviceid
            })
            .collect();

        if physical.is_empty() {
            warn!("no physical pointer devices found; scroll wheel bindings will not work");
            return Ok(None);
        }

        let mask = xinput::XIEventMask::BUTTON_PRESS | xinput::XIEventMask::BUTTON_RELEASE;

        // The server delivers nothing unless events are selected on the root
        // before the grabs are established.
        conn.xinput_xi_select_events(
            root,
            &[EventMask {
                deviceid: xinput::Device::ALL.into(),
                mask: vec![mask],
            }],
        )?
        .check()
        .context("failed to select XInput2 events on root")?;

        for button in [4u32, 5u32] {
            let reply = conn
                .xinput_xi_passive_grab_device(
                    x11rb::CURRENT_TIME,
                    root,
                    0, // no cursor change
                    button,
                    pointer,
                    xinput::GrabType::BUTTON,
                    // SYNC freezes the device until XIAllowEvents decides
                    // between REPLAY_DEVICE (passthrough) and ASYNC_DEVICE (block).
                    xinput::GrabMode22::SYNC,
                    x11rb::protocol::xproto::GrabMode::ASYNC,
                    xinput::GrabOwner::OWNER,
                    &[u32::from(mask)],
                    &[0], // any modifier
                )?
                .reply()?;

            if let Some(status) = reply.modifiers.first()
                && status.status != GrabStatus::SUCCESS
            {
                warn!("failed to grab button {}: {:?}", button, status.status);
            }
        }
        conn.flush()?;

        Ok(Some(Self {
            pointer,
            root,
            physical,
        }))
    }

    /// Map a button press to a scroll direction, ignoring synthetic sources.
    fn classify(&self, ev: &x11rb::protocol::xinput::ButtonPressEvent) -> Option<bool> {
        if !self.physical.contains(&ev.sourceid) {
            return None;
        }
        match ev.detail {
            4 => Some(true),
            5 => Some(false),
            _ => None,
        }
    }

    /// Thaw the frozen device, either replaying the event or consuming it.
    fn allow(&self, conn: &x11rb::rust_connection::RustConnection, replay: bool) {
        use x11rb::connection::Connection;
        use x11rb::protocol::xinput::{ConnectionExt as _, EventMode};

        let mode = if replay {
            EventMode::REPLAY_DEVICE
        } else {
            EventMode::ASYNC_DEVICE
        };

        if let Err(e) =
            conn.xinput_xi_allow_events(x11rb::CURRENT_TIME, self.pointer, mode, 0, self.root)
        {
            warn!("failed to thaw scroll event: {}", e);
        }
        if let Err(e) = conn.flush() {
            warn!("failed to flush X11 connection: {}", e);
        }
    }
}

/// Whether a device should be taken over exclusively.
///
/// Only devices that can produce a bound key are claimed. Motion devices never
/// are: re-injecting REL_X/REL_Y loses the DPI properties libinput accelerates
/// with, which changes how the mouse feels.
fn should_grab_device(path: &Path, wanted: &HashSet<evdev::KeyCode>) -> bool {
    let Ok(device) = Device::open(path) else {
        return false;
    };

    if device.name() == Some(VIRTUAL_KEYBOARD_NAME) {
        return false;
    }

    let has_motion = device
        .supported_relative_axes()
        .map(|axes| axes.contains(RelativeAxisCode::REL_X))
        .unwrap_or(false);

    if has_motion {
        return false;
    }

    device
        .supported_keys()
        .map(|keys| wanted.iter().any(|key| keys.contains(*key)))
        .unwrap_or(false)
}

/// Find every device that can produce a bound key
fn find_bindable_devices(wanted: &HashSet<evdev::KeyCode>) -> Result<Vec<PathBuf>> {
    let mut devices = Vec::new();

    for entry in std::fs::read_dir("/dev/input").context("failed to read /dev/input directory")? {
        let path = entry?.path();

        let is_event_node = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"));

        if is_event_node && should_grab_device(&path, wanted) {
            devices.push(path);
        }
    }

    Ok(devices)
}

/// Grab a device and spawn its reader task, recording it as held on success.
fn grab_and_spawn(
    path: &Path,
    held: &mut HashSet<PathBuf>,
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

    if let Err(e) = device.grab() {
        // Contention is normal and often transient (another remapper, or a
        // node that is not settled yet), so the periodic re-scan retries.
        debug!("failed to grab {} ({}): {}", name, path.display(), e);
        return;
    }

    info!("grabbed device: {} ({})", name, path.display());
    held.insert(path.to_path_buf());

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

/// Create a virtual keyboard for re-injecting events
///
/// Keys only: relative axes make udev tag the device a mouse, which splits it
/// into a pointer plus keyboard subdevice under libinput. Scroll passthrough
/// replays the X11 grab rather than re-injecting.
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
