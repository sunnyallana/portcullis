//! Core domain types for Portcullis.
//!
//! This crate holds the value model, the live-schema description, the action
//! specification that operators write, caller identity, column masking and the
//! hash-chained audit record. It performs no I/O and contains no SQL: filter and
//! column expressions travel as strings and are parsed by `portcullis-sql`.

pub mod audit;
pub mod branding;
pub mod caller;
pub mod mask;
pub mod schema;
pub mod spec;
pub mod value;

mod error;
mod hex;

pub use audit::{AuditLog, AuditRecord, Decision, Fsync};
pub use branding::{BIN, NAME};
pub use caller::Caller;
pub use error::{Error, Result};
pub use mask::Mask;
pub use schema::{Column, Schema, Table, did_you_mean};
pub use spec::{ActionKind, ActionSpec, ApprovalRule, OrderTerm, ParamSpec, WriteMode, WriteSpec};
pub use value::{DataType, Value};
