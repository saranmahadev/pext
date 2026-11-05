//! pext-core: pure domain logic. No I/O, no floating point in money paths.
#![deny(clippy::float_arithmetic)]
#![allow(clippy::result_large_err)]

pub mod dates;
pub mod error;
pub mod invest;
pub mod loan;
pub mod money;
pub mod qlang;
pub mod query;
pub mod xirr;

pub use error::{Error, ErrorKind, Result};
