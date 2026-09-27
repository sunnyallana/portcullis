//! The Portcullis engine: configuration, validation and the request path.
//!
//! A deployment is one [`Config`] file plus a backend. [`Engine::build`]
//! validates every action against the live schema before anything is published,
//! and [`Engine::call`] is the single entry point every caller goes through.

pub mod approvals;
pub mod config;
pub mod engine;
pub mod limits;
pub mod profile;
pub mod registry;
pub mod replay;

pub use approvals::{Approval, ApprovalStore, Status};
pub use config::{BackendConfig, Config, LimitStore, Limits, Role};
pub use engine::{CallResult, Engine};
pub use profile::{Classification, ColumnProfile, Profile, TableProfile};
pub use registry::{Action, Registry, Warning};
pub use replay::{Baseline, Report, Verdict};
