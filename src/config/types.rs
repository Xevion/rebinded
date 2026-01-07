//! Configuration type definitions
//!
//! Contains the data structures representing parsed configuration.

use serde::Deserialize;
use std::hash::{Hash, Hasher};
use std::ops::Range;

/// Byte span in the source file
pub type Span = Range<usize>;

/// A value with its source location
///
/// Hash and Eq are implemented based on the value only, ignoring the span.
/// This allows using Spanned<T> as HashMap keys where semantically equal
/// values (regardless of where they appear in source) should be treated as equal.
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    value: T,
    span: Span,
}

impl<T> Spanned<T> {
    pub fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn span(&self) -> &Span {
        &self.span
    }
}

impl<T> std::ops::Deref for Spanned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// Hash based on value only - span is metadata, not identity
impl<T: Hash> Hash for Spanned<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

/// Equality based on value only - span is metadata, not identity
impl<T: PartialEq> PartialEq for Spanned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq> Eq for Spanned<T> {}

use std::collections::HashMap;

/// Strategy configuration variants
///
/// Each variant corresponds to a strategy implementation. The `type` field
/// in TOML determines which variant is used.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StrategyConfig {
    /// Gated hold: require hold before activation, with repeat window
    GatedHold {
        /// How long the key must be held before first activation (ms)
        initial_hold_ms: u64,
        /// Window during which repeated presses activate immediately (ms)
        repeat_window_ms: u64,
        /// Events that divert the strategy to alternative actions.
        /// Keys are event identifiers (e.g., "scroll_up", "scroll_down"),
        /// values are action names (e.g., "volume_up", "volume_down").
        #[serde(default)]
        diverts: HashMap<String, String>,
    },
}

/// A key binding configuration
#[derive(Debug, Clone)]
pub struct Binding {
    /// The action(s) to perform
    pub action: ActionSpec,
    /// Optional reference to a named strategy (with span for error reporting)
    pub strategy: Option<Spanned<String>>,
}

/// Action specification - either simple or conditional
#[derive(Debug, Clone)]
pub enum ActionSpec {
    /// Simple action with no conditions
    Simple(Action),
    /// List of conditional rules, evaluated in order
    Conditional(Vec<ConditionalAction>),
}

/// A conditional action rule
#[derive(Debug, Clone, Deserialize)]
pub struct ConditionalAction {
    #[serde(default)]
    pub condition: Condition,
    pub action: Action,
}

/// Window matching condition - all fields are ANDed together
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Condition {
    #[serde(default)]
    pub window: WindowCondition,
}

impl Condition {
    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }
}

/// Conditions for matching the active window
/// Supports both positive matches (title, class, binary) and negations (not_title, not_class, not_binary)
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WindowCondition {
    /// Glob pattern to match window title
    pub title: Option<String>,
    /// Glob pattern that must NOT match window title
    pub not_title: Option<String>,
    /// Glob pattern to match window class (X11 WM_CLASS / Windows class name)
    pub class: Option<String>,
    /// Glob pattern that must NOT match window class
    pub not_class: Option<String>,
    /// Glob pattern to match executable name (without path)
    pub binary: Option<String>,
    /// Glob pattern that must NOT match executable name
    pub not_binary: Option<String>,
}

impl WindowCondition {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.not_title.is_none()
            && self.class.is_none()
            && self.not_class.is_none()
            && self.binary.is_none()
            && self.not_binary.is_none()
    }

    /// Check if the condition matches the given window info
    /// All specified fields must match (AND logic)
    pub fn matches(&self, info: &WindowInfo) -> bool {
        let matches_glob =
            |pattern: &str, value: &str| -> bool { glob_match::glob_match(pattern, value) };

        // Positive matches: if specified, must match
        if let Some(ref pattern) = self.title
            && !matches_glob(pattern, &info.title)
        {
            return false;
        }
        if let Some(ref pattern) = self.class
            && !matches_glob(pattern, &info.class)
        {
            return false;
        }
        if let Some(ref pattern) = self.binary
            && !matches_glob(pattern, &info.binary)
        {
            return false;
        }

        // Negative matches: if specified, must NOT match
        if let Some(ref pattern) = self.not_title
            && matches_glob(pattern, &info.title)
        {
            return false;
        }
        if let Some(ref pattern) = self.not_class
            && matches_glob(pattern, &info.class)
        {
            return false;
        }
        if let Some(ref pattern) = self.not_binary
            && matches_glob(pattern, &info.binary)
        {
            return false;
        }

        true
    }
}

/// Information about the currently focused window (filled by platform layer)
#[derive(Debug, Default)]
pub struct WindowInfo {
    pub title: String,
    pub class: String,
    pub binary: String,
}

/// Available actions that can be bound to keys
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    // Media actions
    MediaPlayPause,
    MediaNext,
    MediaPrevious,
    MediaStop,

    // Volume actions
    VolumeUp,
    VolumeDown,
    VolumeMute,

    // Browser actions
    BrowserBack,
    BrowserForward,

    // Pass the key through unchanged
    Passthrough,

    // Block the key entirely
    Block,
}

impl Action {
    /// Execute this action using the platform.
    ///
    /// This method is primarily used in tests and for direct platform execution.
    /// In normal operation, prefer using `PlatformHandle::execute` or `StrategyContext::execute`.
    ///
    /// Note: `Passthrough` and `Block` are handled at the event loop level,
    /// not here - calling execute on them is a no-op.
    #[allow(dead_code)] // Public API for tests and direct platform usage
    pub fn execute(&self, platform: &impl crate::platform::PlatformInterface) {
        use crate::platform::{MediaCommand, SyntheticKey};
        use tracing::debug;

        debug!(?self, "executing action");

        match self {
            Action::MediaPlayPause => platform.send_media(MediaCommand::PlayPause),
            Action::MediaNext => platform.send_media(MediaCommand::Next),
            Action::MediaPrevious => platform.send_media(MediaCommand::Previous),
            Action::MediaStop => platform.send_media(MediaCommand::Stop),
            Action::VolumeUp => platform.send_media(MediaCommand::VolumeUp),
            Action::VolumeDown => platform.send_media(MediaCommand::VolumeDown),
            Action::VolumeMute => platform.send_media(MediaCommand::VolumeMute),
            Action::BrowserBack => platform.send_key(SyntheticKey::BrowserBack),
            Action::BrowserForward => platform.send_key(SyntheticKey::BrowserForward),
            Action::Passthrough | Action::Block => {}
        }
    }

    /// Returns the corresponding EventResponse for non-executable actions.
    ///
    /// - `Passthrough` → `Some(EventResponse::Passthrough)`
    /// - `Block` → `Some(EventResponse::Block)`
    /// - All other actions → `None` (must be executed via platform)
    pub fn as_response(&self) -> Option<crate::platform::EventResponse> {
        match self {
            Action::Passthrough => Some(crate::platform::EventResponse::Passthrough),
            Action::Block => Some(crate::platform::EventResponse::Block),
            _ => None,
        }
    }
}
