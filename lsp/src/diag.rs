// Copyright 2026 Oxide Computer Company

//! Diagnostics shared by every analysis phase.

use crate::source::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub severity: Severity,
    pub message: String,
    /// Extra locations relevant to the diagnostic, such as a previous
    /// declaration of a duplicated name.
    pub related: Vec<(Span, String)>,
    /// Marks code that is unused or inactive so editors can fade it.
    pub unnecessary: bool,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Self::new(span, Severity::Error, message)
    }

    pub fn warning(span: Span, message: impl Into<String>) -> Self {
        Self::new(span, Severity::Warning, message)
    }

    pub fn new(
        span: Span,
        severity: Severity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            span,
            severity,
            message: message.into(),
            related: Vec::new(),
            unnecessary: false,
        }
    }

    pub fn with_related(
        mut self,
        span: Span,
        message: impl Into<String>,
    ) -> Self {
        self.related.push((span, message.into()));
        self
    }
}
