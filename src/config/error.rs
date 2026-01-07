//! Configuration error types with source location tracking
//!
//! Provides rich diagnostic output using miette for configuration validation errors.

// False positives from miette's derive macros - fields are used but rustc doesn't see it
#![allow(unused_assignments)]

use super::types::Span;
use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

/// Convert byte offset to 1-based line number
pub fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    content[..offset.min(content.len())]
        .chars()
        .filter(|&c| c == '\n')
        .count()
        + 1
}

/// A single validation issue with location information
#[derive(Debug, Clone)]
pub struct ConfigIssue {
    /// Byte span in source
    pub span: Span,
    /// Primary error message
    pub message: String,
    /// Label shown at the span location
    pub label: String,
    /// Optional help text with suggestions
    pub help: Option<String>,
}

impl ConfigIssue {
    /// Create an issue for an unresolvable key name
    pub fn unknown_key(span: Span, key: &str) -> Self {
        Self {
            span,
            message: format!("unknown key '{key}'"),
            label: "not a valid key name or code".to_string(),
            help: Some(
                "valid formats: hex (0x7C), decimal (124), or key name (space, enter)".to_string(),
            ),
        }
    }

    /// Create an issue for a reference to an undefined strategy
    pub fn undefined_strategy(span: Span, name: &str, defined: &[&str]) -> Self {
        let help = if defined.is_empty() {
            "no strategies are defined in this config".to_string()
        } else {
            format!("defined strategies: {}", defined.join(", "))
        };
        Self {
            span,
            message: format!("undefined strategy '{name}'"),
            label: "strategy not found".to_string(),
            help: Some(help),
        }
    }

    /// Create an issue for a duplicate key binding
    pub fn duplicate_binding(
        span: Span,
        key_display: &str,
        original_span: Span,
        source_content: &str,
    ) -> Self {
        let original_line = byte_offset_to_line(source_content, original_span.start);
        Self {
            span,
            message: format!("duplicate binding for key '{key_display}'"),
            label: "duplicate".to_string(),
            help: Some(format!("first defined at line {original_line}")),
        }
    }
}

/// Individual validation issue wrapped for miette's `#[related]` attribute
#[derive(Debug, Error, Diagnostic)]
#[error("{message}")]
#[allow(unused_assignments)] // Fields used by miette's derive macros
pub struct ConfigIssueDiagnostic {
    message: String,
    #[label("{label}")]
    span: SourceSpan,
    label: String,
    #[help]
    help: Option<String>,
}

/// Collection of configuration validation errors
///
/// This is the main diagnostic type returned when config validation fails.
/// It contains the source file and all issues found, sorted by position.
#[derive(Debug, Error, Diagnostic)]
#[error(
    "configuration has {count} error{s}",
    count = self.issues.len(),
    s = if self.issues.len() == 1 { "" } else { "s" }
)]
#[diagnostic(code(rebinded::config::validation))]
#[allow(unused_assignments)] // Fields used by miette's derive macros
pub struct ConfigValidationError {
    #[source_code]
    src: NamedSource<String>,

    #[related]
    issues: Vec<ConfigIssueDiagnostic>,
}

impl ConfigValidationError {
    /// Create a validation error from collected issues
    ///
    /// Issues are sorted by source position for deterministic output.
    #[allow(unused_assignments)] // Field assignments used by miette's derive macros
    pub fn new(
        source_name: impl Into<String>,
        source_content: String,
        mut issues: Vec<ConfigIssue>,
    ) -> Self {
        // Sort by span start for deterministic, readable output
        issues.sort_by_key(|i| i.span.start);

        let diagnostics = issues
            .into_iter()
            .map(|issue| ConfigIssueDiagnostic {
                message: issue.message,
                span: (issue.span.start, issue.span.len()).into(),
                label: issue.label,
                help: issue.help,
            })
            .collect();

        let name: String = source_name.into();
        Self {
            src: NamedSource::new(name, source_content),
            issues: diagnostics,
        }
    }
}

/// Top-level configuration errors
#[derive(Debug, Error, Diagnostic)]
pub enum ConfigError {
    #[error("failed to read config file: {path}")]
    #[diagnostic(code(rebinded::config::io))]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config")]
    #[diagnostic(code(rebinded::config::parse))]
    #[allow(unused_assignments)] // Fields used by miette's derive macros
    Parse {
        #[source_code]
        src: NamedSource<String>,
        #[label("parse error")]
        span: Option<SourceSpan>,
        msg: String,
    },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Validation(#[from] ConfigValidationError),
}

impl ConfigError {
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    #[allow(unused_assignments)] // Field assignments used by miette's derive macros
    pub fn parse(
        source_name: impl Into<String>,
        source_content: String,
        err: toml::de::Error,
    ) -> Self {
        let name: String = source_name.into();
        Self::Parse {
            src: NamedSource::new(name, source_content),
            span: err.span().map(|r| (r.start, r.len()).into()),
            msg: err.message().to_string(),
        }
    }
}
