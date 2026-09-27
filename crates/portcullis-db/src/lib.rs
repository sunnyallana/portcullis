//! Database backends.
//!
//! A backend owns its connections, reports the live schema and executes plans.
//! Four ship with Portcullis: PostgreSQL, MySQL and SQL Server over pooled
//! connections, and
//! an in-memory backend used by the test suite and by `portcullis init` so the
//! product can be tried without provisioning anything.

pub mod memory;
#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "mysql")]
pub mod mysql;
pub mod plan;
#[cfg(feature = "postgres")]
pub mod postgres;

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use portcullis_core::{Result, Schema};

pub use memory::MemoryBackend;
#[cfg(feature = "mssql")]
pub use mssql::MsSqlBackend;
#[cfg(feature = "mysql")]
pub use mysql::MySqlBackend;
pub use plan::{ExecCtx, ReadPlan, Rows, WriteOutcome, WritePlan};
#[cfg(feature = "postgres")]
pub use postgres::PostgresBackend;

/// A place Portcullis can read from and write to.
#[async_trait]
pub trait Backend: fmt::Debug + Send + Sync {
    /// Short description for logs and `portcullis doctor`, with no credentials in it.
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

    /// Can this backend hold rate-limit counters and replay keys for the whole
    /// deployment rather than one process?
    ///
    /// False by default. A backend says true only once its tables exist, so
    /// the engine can refuse at startup rather than at the first write.
    async fn supports_shared_state(&self) -> Result<bool> {
        Ok(false)
    }

    /// Count one call and report whether it is within the limit.
    ///
    /// Deliberately narrow rather than a general "run this SQL" hatch: the
    /// claim that caller input only ever becomes a bind parameter depends on
    /// there being no such hatch.
    async fn rate_check(&self, _caller: &str, _action: &str, _per_minute: u32) -> Result<bool> {
        Err(unsupported("rate limiting"))
    }

    /// The response of a previous identical write, if one is remembered.
    async fn replay_get(&self, _key: &str, _ttl: Duration) -> Result<Option<serde_json::Value>> {
        Err(unsupported("replay protection"))
    }

    /// Remember a completed write, and forget anything past the window.
    async fn replay_put(
        &self,
        _key: &str,
        _action: &str,
        _response: &serde_json::Value,
        _ttl: Duration,
    ) -> Result<()> {
        Err(unsupported("replay protection"))
    }
}

fn unsupported(what: &str) -> portcullis_core::Error {
    portcullis_core::Error::Config(format!(
        "this backend cannot hold shared {what}; set [limits] store = \"local\" or use PostgreSQL or MySQL"
    ))
}
