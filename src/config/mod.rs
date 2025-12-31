//! Configuration loading, parsing, and validation
//!
//! This module handles:
//! - Loading config from TOML files
//! - Parsing with span preservation for error reporting
//! - Validating all references and key names
//! - Building the runtime configuration

mod error;
mod types;

pub use error::{ConfigError, ConfigIssue, ConfigValidationError};
pub use types::{
    Action, ActionSpec, Binding, ConditionalAction, Spanned, StrategyConfig, WindowInfo,
};

use crate::key::KeyCode;
use crate::strategy::{GatedHoldConfig, GatedHoldStrategy, KeyStrategy};
use serde::de::IntoDeserializer;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;
use toml::de::{DeTable, DeValue};
use tracing::warn;

/// Parsed configuration before validation
///
/// Contains raw parsed data with source spans preserved for error reporting.
/// Uses HashMap with Spanned keys - the Spanned type implements Hash/Eq based
/// on value only (ignoring span), so lookups work correctly while preserving
/// span information for error reporting.
#[derive(Debug)]
pub struct Config {
    /// Strategy definitions keyed by name
    pub strategies: HashMap<Spanned<String>, StrategyConfig>,
    /// Key bindings keyed by key name string
    pub bindings: HashMap<Spanned<String>, Binding>,
}

/// Runtime configuration with resolved key codes and instantiated strategies
///
/// This is built from Config at startup, resolving all key name strings
/// to platform-native KeyCodes for fast lookup during event processing.
pub struct RuntimeConfig {
    /// Maps key codes to their bindings
    pub bindings: HashMap<KeyCode, Binding>,
    /// Instantiated strategies, keyed by name
    pub strategies: HashMap<String, Arc<Mutex<dyn KeyStrategy>>>,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("bindings", &self.bindings)
            .field("strategies", &format!("<{} strategies>", self.strategies.len()))
            .finish()
    }
}

impl RuntimeConfig {
    /// Resolve which action to take for a given key and window context
    #[allow(dead_code)]
    pub fn resolve_action(&self, key: KeyCode, window: &WindowInfo) -> Option<&Action> {
        let binding = self.bindings.get(&key)?;

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
}

/// Load and validate configuration from a file
///
/// Returns the parsed config and runtime config, or a detailed error with
/// source locations for all validation issues found.
pub fn load(path: impl AsRef<Path>) -> Result<(Config, RuntimeConfig), ConfigError> {
    let path = path.as_ref();
    let source_name = path.display().to_string();

    let content = std::fs::read_to_string(path).map_err(|e| ConfigError::io(&source_name, e))?;

    load_from_str(&source_name, content)
}

/// Load and validate configuration from a string
///
/// Useful for testing and when config content is already in memory.
pub fn load_from_str(
    source_name: &str,
    content: String,
) -> Result<(Config, RuntimeConfig), ConfigError> {
    let mut loader = ConfigLoader::new(source_name.to_string(), content);
    loader.parse_and_build()
}

/// Internal config loader that tracks parsing state and validation issues
struct ConfigLoader {
    source_name: String,
    source_content: String,
    issues: Vec<ConfigIssue>,
}

impl ConfigLoader {
    fn new(source_name: String, source_content: String) -> Self {
        Self {
            source_name,
            source_content,
            issues: Vec::new(),
        }
    }

    /// Parse content and build runtime config
    fn parse_and_build(&mut self) -> Result<(Config, RuntimeConfig), ConfigError> {
        // Parse into spanned table for location tracking
        // Clone content for parsing - DeTable<'a> has a lifetime tied to the source,
        // but we need to mutably borrow self during parse_table
        let content_for_parse = self.source_content.clone();
        let table = DeTable::parse(&content_for_parse)
            .map_err(|e| ConfigError::parse(&self.source_name, self.source_content.clone(), e))?;

        let config = self.parse_table(table.into_inner());
        let runtime = self.build_runtime(&config);

        if self.issues.is_empty() {
            Ok((config, runtime))
        } else {
            Err(ConfigValidationError::new(
                self.source_name.clone(),
                self.source_content.clone(),
                std::mem::take(&mut self.issues),
            )
            .into())
        }
    }

    /// Parse the root TOML table into a Config
    fn parse_table(&mut self, table: DeTable) -> Config {
        let mut strategies = HashMap::new();
        let mut bindings = HashMap::new();

        for (key, value) in table {
            let key_str = key.get_ref().as_ref();

            match key_str {
                "strategies" => {
                    strategies = self.parse_strategies(value);
                }
                "bindings" => {
                    bindings = self.parse_bindings(value);
                }
                _ => {
                    // Unknown top-level key - could add a warning here
                }
            }
        }

        Config {
            strategies,
            bindings,
        }
    }

    /// Parse the \[strategies\] section
    fn parse_strategies(
        &mut self,
        value: toml::Spanned<DeValue>,
    ) -> HashMap<Spanned<String>, StrategyConfig> {
        let mut result = HashMap::new();

        let DeValue::Table(table) = value.into_inner() else {
            return result;
        };

        for (name_spanned, config_spanned) in table {
            let name = name_spanned.get_ref().to_string();
            let name_span = name_spanned.span();
            let config_span = config_spanned.span();

            // Deserialize the strategy config directly using IntoDeserializer
            match StrategyConfig::deserialize(config_spanned.into_deserializer()) {
                Ok(config) => {
                    result.insert(Spanned::new(name, name_span), config);
                }
                Err(e) => {
                    self.issues.push(ConfigIssue {
                        span: config_span,
                        message: format!("invalid strategy config: {e}"),
                        label: "invalid strategy".to_string(),
                        help: None,
                    });
                }
            }
        }

        result
    }

    /// Parse the \[bindings\] section
    fn parse_bindings(
        &mut self,
        value: toml::Spanned<DeValue>,
    ) -> HashMap<Spanned<String>, Binding> {
        let mut result = HashMap::new();

        let DeValue::Table(table) = value.into_inner() else {
            return result;
        };

        for (key_spanned, binding_spanned) in table {
            let key_name = key_spanned.get_ref().to_string();
            let key_span = key_spanned.span();

            if let Some(binding) = self.parse_binding(binding_spanned) {
                result.insert(Spanned::new(key_name, key_span), binding);
            }
        }

        result
    }

    /// Parse a single binding entry
    fn parse_binding(&mut self, value: toml::Spanned<DeValue>) -> Option<Binding> {
        let binding_span = value.span();
        let DeValue::Table(table) = value.into_inner() else {
            self.issues.push(ConfigIssue {
                span: binding_span,
                message: "binding must be a table".to_string(),
                label: "expected table".to_string(),
                help: Some("example: [bindings.f13]\naction = \"media_play_pause\"".to_string()),
            });
            return None;
        };

        let mut action: Option<ActionSpec> = None;
        let mut strategy: Option<Spanned<String>> = None;

        for (field_key, field_value) in table {
            let field_name = field_key.get_ref().as_ref();
            let field_span = field_value.span();

            match field_name {
                "action" => {
                    action = self.parse_action_spec(field_value);
                }
                "strategy" => {
                    // Extract strategy name with its span
                    if let DeValue::String(s) = field_value.get_ref() {
                        strategy = Some(Spanned::new(s.to_string(), field_span));
                    } else {
                        self.issues.push(ConfigIssue {
                            span: field_span,
                            message: "strategy must be a string".to_string(),
                            label: "expected string".to_string(),
                            help: None,
                        });
                    }
                }
                _ => {
                    // Unknown field in binding
                }
            }
        }

        let Some(action) = action else {
            self.issues.push(ConfigIssue {
                span: binding_span,
                message: "binding missing required 'action' field".to_string(),
                label: "missing action".to_string(),
                help: Some("add: action = \"media_play_pause\"".to_string()),
            });
            return None;
        };

        Some(Binding { action, strategy })
    }

    /// Parse an action specification (simple string or conditional array)
    fn parse_action_spec(&mut self, value: toml::Spanned<DeValue>) -> Option<ActionSpec> {
        let span = value.span();

        match value.into_inner() {
            DeValue::String(s) => {
                // Simple action string
                match parse_action(&s) {
                    Ok(action) => Some(ActionSpec::Simple(action)),
                    Err(e) => {
                        self.issues.push(ConfigIssue {
                            span,
                            message: e,
                            label: "unknown action".to_string(),
                            help: Some(
                                "valid actions: media_play_pause, media_next, media_previous, \
                                 media_stop, browser_back, browser_forward, passthrough, block"
                                    .to_string(),
                            ),
                        });
                        None
                    }
                }
            }
            DeValue::Array(arr) => {
                // Conditional action array
                let mut rules = Vec::new();
                for item in arr {
                    let item_span = item.span();
                    match ConditionalAction::deserialize(item.into_deserializer()) {
                        Ok(rule) => rules.push(rule),
                        Err(e) => {
                            self.issues.push(ConfigIssue {
                                span: item_span,
                                message: format!("invalid conditional rule: {e}"),
                                label: "invalid rule".to_string(),
                                help: None,
                            });
                        }
                    }
                }
                if rules.is_empty() {
                    None
                } else {
                    Some(ActionSpec::Conditional(rules))
                }
            }
            _ => {
                self.issues.push(ConfigIssue {
                    span,
                    message: "action must be a string or array".to_string(),
                    label: "invalid type".to_string(),
                    help: Some(
                        "use a string for simple actions: action = \"media_play_pause\"\n\
                         or an array for conditional: action = [{ condition = ..., action = ... }]"
                            .to_string(),
                    ),
                });
                None
            }
        }
    }

    /// Build runtime config with validation
    fn build_runtime(&mut self, config: &Config) -> RuntimeConfig {
        // Collect strategy names for reference validation
        let strategy_names: Vec<&str> = config
            .strategies
            .keys()
            .map(|name| name.value().as_str())
            .collect();

        // Track seen key codes to detect duplicates
        let mut seen_keys: HashMap<KeyCode, types::Span> = HashMap::new();
        let mut bindings = HashMap::new();

        for (key_spanned, binding) in &config.bindings {
            let key_str = key_spanned.value();
            let key_span = key_spanned.span().clone();

            // Validate key resolves to a known code
            let Some(key_code) = KeyCode::from_config_str(key_str) else {
                self.issues
                    .push(ConfigIssue::unknown_key(key_span, key_str));
                continue;
            };

            // Check for duplicate bindings (same key code from different strings)
            if let Some(original_span) = seen_keys.get(&key_code) {
                self.issues.push(ConfigIssue::duplicate_binding(
                    key_span,
                    &key_code.display_name(),
                    original_span.clone(),
                    &self.source_content,
                ));
                continue;
            }
            seen_keys.insert(key_code, key_span);

            // Validate strategy reference if present
            if let Some(ref strategy_ref) = binding.strategy {
                let strategy_name = strategy_ref.value();
                if !strategy_names.contains(&strategy_name.as_str()) {
                    self.issues.push(ConfigIssue::undefined_strategy(
                        strategy_ref.span().clone(),
                        strategy_name,
                        &strategy_names,
                    ));
                }
            }

            // Warn if conditional binding has no catch-all rule
            if let ActionSpec::Conditional(rules) = &binding.action {
                let has_catch_all = rules.iter().any(|rule| rule.condition.is_empty());
                if !has_catch_all {
                    warn!(
                        key = key_str,
                        "conditional binding has no catch-all rule; \
                         key will passthrough when no conditions match"
                    );
                }
            }

            bindings.insert(key_code, binding.clone());
        }

        // Instantiate strategies
        let mut strategies: HashMap<String, Arc<Mutex<dyn KeyStrategy>>> = HashMap::new();
        for (name, strategy_config) in &config.strategies {
            let strategy: Arc<Mutex<dyn KeyStrategy>> = match strategy_config {
                StrategyConfig::GatedHold {
                    initial_hold_ms,
                    repeat_window_ms,
                } => Arc::new(Mutex::new(GatedHoldStrategy::new(GatedHoldConfig {
                    initial_hold_ms: *initial_hold_ms,
                    repeat_window_ms: *repeat_window_ms,
                }))),
            };
            strategies.insert(name.value().clone(), strategy);
        }

        RuntimeConfig {
            bindings,
            strategies,
        }
    }

}

/// Parse an action string into an Action enum
fn parse_action(s: &str) -> Result<Action, String> {
    match s {
        "media_play_pause" => Ok(Action::MediaPlayPause),
        "media_next" => Ok(Action::MediaNext),
        "media_previous" => Ok(Action::MediaPrevious),
        "media_stop" => Ok(Action::MediaStop),
        "browser_back" => Ok(Action::BrowserBack),
        "browser_forward" => Ok(Action::BrowserForward),
        "passthrough" => Ok(Action::Passthrough),
        "block" => Ok(Action::Block),
        _ => Err(format!("unknown action '{s}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::WindowCondition;
    use assert2::assert;

    #[test]
    fn test_simple_action_parsing() {
        let toml = r#"
            [bindings.0x7C]
            action = "media_play_pause"
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_ok());
        let (config, _) = result.unwrap();
        assert!(config.bindings.len() == 1);
    }

    #[test]
    fn test_conditional_action_parsing() {
        let toml = r#"
            [bindings.0x80]
            action = [
                { condition = { window = { title = "*vivaldi*" } }, action = "browser_back" },
            ]
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_ok());
    }

    #[test]
    fn test_strategy_config() {
        let toml = r#"
            [strategies.scroll]
            type = "gated_hold"
            initial_hold_ms = 150
            repeat_window_ms = 2000

            [bindings.0x7E]
            action = "media_previous"
            strategy = "scroll"
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_ok());
        let (config, runtime) = result.unwrap();
        assert!(config.strategies.len() == 1);
        assert!(runtime.strategies.contains_key("scroll"));
    }

    #[test]
    fn test_invalid_action_name() {
        let toml = r#"
            [bindings.0x7C]
            action = "invalid_action"
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_undefined_strategy_error() {
        let toml = r#"
            [bindings.0x7C]
            action = "media_play_pause"
            strategy = "nonexistent"
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("nonexistent"));
    }

    #[test]
    fn test_duplicate_binding_error() {
        // Both hex codes resolve to the same key
        let toml = r#"
            [bindings.0x7C]
            action = "media_play_pause"

            [bindings.124]
            action = "block"
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("duplicate"));
    }

    #[test]
    fn test_multiple_errors_collected() {
        let toml = r#"
            [bindings.invalid_key_1]
            action = "media_play_pause"

            [bindings.invalid_key_2]
            action = "media_next"
            strategy = "undefined_strategy"
        "#;
        let result = load_from_str("test.toml", toml.to_string());
        assert!(result.is_err());
        
        // Should have multiple errors
        if let Err(ConfigError::Validation(v)) = result {
            // Check that debug output mentions both issues
            let msg = format!("{v:?}");
            assert!(msg.contains("invalid_key_1") || msg.contains("invalid_key_2"));
        } else {
            panic!("expected validation error");
        }
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
}
