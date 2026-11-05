//! Network-facing front ends for pext: an MCP server (stdio), a local HTTP API
//! and a small web dashboard. All of them go through [`Gateway`], which decides
//! what a caller may see and do; the engine still enforces roles, scopes and
//! redaction underneath.

#![forbid(unsafe_code)]
#![allow(clippy::result_large_err)]

pub mod gateway;
pub mod http;
pub mod mcp;

pub use gateway::{CallFlags, Gateway};
