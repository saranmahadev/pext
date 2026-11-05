//! Structured errors shared by every layer. Each error has a stable machine
//! code, a kind that maps to a process exit code, and an optional hint/field.

use serde_json::{json, Value};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    Validation,
    NotFound,
    Conflict,
    Auth,
    Io,
    Integrity,
    Internal,
}

impl ErrorKind {
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Usage => 2,
            ErrorKind::Validation => 3,
            ErrorKind::NotFound => 4,
            ErrorKind::Conflict => 5,
            ErrorKind::Auth => 6,
            ErrorKind::Io => 7,
            ErrorKind::Integrity => 8,
            ErrorKind::Internal => 70,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::Validation => "validation",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Auth => "auth",
            ErrorKind::Io => "io",
            ErrorKind::Integrity => "integrity",
            ErrorKind::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    pub field: Option<String>,
    pub details: Option<Value>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: ErrorKind, code: &str, message: impl Into<String>) -> Self {
        Error {
            kind,
            code: code.to_string(),
            message: message.into(),
            hint: None,
            field: None,
            details: None,
        }
    }
    pub fn usage(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, code, message)
    }
    pub fn validation(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Validation, code, message)
    }
    pub fn not_found(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, code, message)
    }
    pub fn conflict(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, code, message)
    }
    pub fn auth(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Auth, code, message)
    }
    pub fn io(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, code, message)
    }
    pub fn integrity(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Integrity, code, message)
    }
    pub fn internal(code: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, code, message)
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }
    pub fn to_json(&self) -> Value {
        let mut e = json!({
            "code": self.code,
            "kind": self.kind.as_str(),
            "message": self.message,
        });
        if let Some(h) = &self.hint {
            e["hint"] = json!(h);
        }
        if let Some(f) = &self.field {
            e["field"] = json!(f);
        }
        if let Some(d) = &self.details {
            e["details"] = d.clone();
        }
        json!({ "schema": "pext/v1", "ok": false, "error": e })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " (hint: {h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}
