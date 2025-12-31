use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub debounce: HashMap<String, DebounceProfile>,
    #[serde(default)]
    pub bindings: HashMap<String, Binding>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DebounceProfile {
    /// How long the key must be held before first activation (ms)
    pub initial_hold_ms: u64,
    /// Window during which repeated presses activate immediately (ms)
    pub repeat_window_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct Binding {
    /// Either a simple action string or a list of conditional rules
    #[serde(deserialize_with = "deserialize_action")]
    pub action: ActionSpec,
    /// Optional reference to a debounce profile name
    pub debounce: Option<String>,
}

#[derive(Debug)]
pub enum ActionSpec {
    /// Simple action with no conditions
    Simple(Action),
    /// List of conditional rules, evaluated in order
    Conditional(Vec<ConditionalAction>),
}

#[derive(Debug, Deserialize)]
pub struct ConditionalAction {
    #[serde(default)]
    pub condition: Condition,
    pub action: Action,
}

/// Window matching condition - all fields are ANDed together
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
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
        let matches_glob = |pattern: &str, value: &str| -> bool {
            glob_match::glob_match(pattern, value)
        };

        // Positive matches: if specified, must match
        if let Some(ref pattern) = self.title {
            if !matches_glob(pattern, &info.title) {
                return false;
            }
        }
        if let Some(ref pattern) = self.class {
            if !matches_glob(pattern, &info.class) {
                return false;
            }
        }
        if let Some(ref pattern) = self.binary {
            if !matches_glob(pattern, &info.binary) {
                return false;
            }
        }

        // Negative matches: if specified, must NOT match
        if let Some(ref pattern) = self.not_title {
            if matches_glob(pattern, &info.title) {
                return false;
            }
        }
        if let Some(ref pattern) = self.not_class {
            if matches_glob(pattern, &info.class) {
                return false;
            }
        }
        if let Some(ref pattern) = self.not_binary {
            if matches_glob(pattern, &info.binary) {
                return false;
            }
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    // Media actions
    MediaPlayPause,
    MediaNext,
    MediaPrev,
    MediaStop,

    // Browser actions
    BrowserBack,
    BrowserForward,

    // Pass the key through unchanged
    Passthrough,

    // Block the key entirely
    Block,
}

// Custom deserializer to handle both "action_name" and [{ condition, action }]
fn deserialize_action<'de, D>(deserializer: D) -> Result<ActionSpec, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, IntoDeserializer, Visitor};

    struct ActionSpecVisitor;

    impl<'de> Visitor<'de> for ActionSpecVisitor {
        type Value = ActionSpec;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string action name or array of conditional actions")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            // Delegate to Action's derived Deserialize implementation
            let action = Action::deserialize(value.into_deserializer())?;
            Ok(ActionSpec::Simple(action))
        }

        fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let rules: Vec<ConditionalAction> =
                Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
            Ok(ActionSpec::Conditional(rules))
        }
    }

    deserializer.deserialize_any(ActionSpecVisitor)
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Resolve which action to take for a given key and window context
    pub fn resolve_action(&self, key: &str, window: &WindowInfo) -> Option<&Action> {
        let binding = self.bindings.get(key)?;

        match &binding.action {
            ActionSpec::Simple(action) => Some(action),
            ActionSpec::Conditional(rules) => {
                for rule in rules {
                    if rule.condition.is_empty() || rule.condition.window.matches(window) {
                        return Some(&rule.action);
                    }
                }
                // Implicit passthrough when no rules match
                None
            }
        }
    }

    /// Get the debounce profile for a binding, if any
    pub fn get_debounce(&self, key: &str) -> Option<&DebounceProfile> {
        let binding = self.bindings.get(key)?;
        let profile_name = binding.debounce.as_ref()?;
        self.debounce.get(profile_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::assert;

    #[test]
    fn test_simple_action_parsing() {
        let toml = r#"
            [bindings.f13]
            action = "media_play_pause"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(matches!(
            config.bindings.get("f13").unwrap().action,
            ActionSpec::Simple(Action::MediaPlayPause)
        ));
    }

    #[test]
    fn test_conditional_action_parsing() {
        let toml = r#"
            [bindings.f17]
            action = [
                { condition = { window = { title = "*vivaldi*" } }, action = "browser_back" },
            ]
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(matches!(
            config.bindings.get("f17").unwrap().action,
            ActionSpec::Conditional(_)
        ));
    }

    #[test]
    fn test_window_condition_matching() {
        let condition = WindowCondition {
            title: Some("*vivaldi*".to_string()),
            ..Default::default()
        };

        let matching = WindowInfo {
            title: "GitHub - vivaldi".to_string(),
            ..Default::default()
        };
        let non_matching = WindowInfo {
            title: "Firefox".to_string(),
            ..Default::default()
        };

        assert!(condition.matches(&matching));
        assert!(!condition.matches(&non_matching));
    }

    #[test]
    fn test_negation_condition() {
        let condition = WindowCondition {
            not_binary: Some("*game*".to_string()),
            ..Default::default()
        };

        let browser = WindowInfo {
            binary: "vivaldi".to_string(),
            ..Default::default()
        };
        let game = WindowInfo {
            binary: "somegame".to_string(),
            ..Default::default()
        };

        assert!(condition.matches(&browser));
        assert!(!condition.matches(&game));
    }

    #[test]
    fn test_debounce_profile() {
        let toml = r#"
            [debounce.scroll]
            initial_hold_ms = 150
            repeat_window_ms = 2000

            [bindings.f15]
            action = "media_prev"
            debounce = "scroll"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        let profile = config.get_debounce("f15").unwrap();
        assert!(profile.initial_hold_ms == 150);
        assert!(profile.repeat_window_ms == 2000);
    }

    #[test]
    fn test_invalid_action_name() {
        let toml = r#"
            [bindings.f13]
            action = "invalid_action"
        "#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown variant"));
    }
}
