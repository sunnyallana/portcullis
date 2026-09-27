//! Database backends.
//!
//! A backend owns its connections, reports the live schema and executes plans.
//! Three ship with Sluice: PostgreSQL and MySQL over pooled connections, and
//! an in-memory backend used by the test suite and by `sluice init` so the
//! product can be tried without provisioning anything.

pub mod memory;
#[cfg(feature = "mysql")]
pub mod mysql;
pub mod plan;
#[cfg(feature = "postgres")]
pub mod postgres;

use std::fmt;

use async_trait::async_trait;
use sluice_core::{Result, Schema};

pub use memory::MemoryBackend;
#[cfg(feature = "mysql")]
pub use mysql::MySqlBackend;
pub use plan::{ExecCtx, ReadPlan, Rows, WriteOutcome, WritePlan};
#[cfg(feature = "postgres")]
pub use postgres::PostgresBackend;

/// A place Sluice can read from and write to.
#[async_trait]
pub trait Backend: fmt::Debug + Send + Sync {
    /// Short description for logs and `sluice doctor`, with no credentials in it.
    fn describe(&self) -> String;

    /// Read the live schema.
    ///
    /// Called once at startup and on demand; action validation is checked
    /// against whatever this returns.
    async fn schema(&self) -> Result<Schema>;

    /// Run a read.
    async fn read(&self, plan: &ReadPlan<'_>, ctx: &ExecCtx<'_>) -> Result<Rows>;

    /// Run a write.
    async fn write(&self, plan: &WritePlan<'_>, ctx: &ExecCtx<'_>) -> Result<WriteOutcome>;

    /// Check that the backend is reachable.
    async fn health(&self) -> Result<()>;
}
