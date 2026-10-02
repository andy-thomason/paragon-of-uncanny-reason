//! Diagnostics shared by every stage.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A diagnostic. `at` is a slice of a [`SourceMap`](crate::source::SourceMap)
/// buffer and is resolved to a position with
/// [`SourceMap::locate`](crate::source::SourceMap::locate).
#[derive(Clone, Debug)]
pub struct Diag<'a> {
    pub severity: Severity,
    pub code: Option<&'static str>,
    pub at: &'a str,
    pub message: String,
    /// Related positions, such as a previous definition.
    pub notes: Vec<(&'a str, String)>,
}

impl<'a> Diag<'a> {
    pub fn error(at: &'a str, message: impl Into<String>) -> Self {
        Diag {
            severity: Severity::Error,
            code: None,
            at,
            message: message.into(),
            notes: Vec::new(),
        }
    }

    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }
}

/// Code for constructs that are valid SystemVerilog but not yet implemented
/// by Paragon. This is our own code, not one of Verilator's.
pub const NOT_YET: &str = "NOTYET";
