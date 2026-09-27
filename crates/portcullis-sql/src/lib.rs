//! SQL construction for Portcullis.
//!
//! Two things live here: the small predicate language operators write in
//! `filter` and `row_filter`, and the builder that turns a validated action
//! into a parameterised statement. Nothing in this crate ever concatenates a
//! caller-supplied value into SQL text.

pub mod build;
pub mod dialect;
pub mod expr;

pub use build::{
    Binder, ReadQuery, Statement, WriteQuery, compile_expr, conjunct_equalities, select, write,
};
pub use dialect::{Dialect, MySql, Postgres};
pub use expr::{CmpOp, Expr, Term, parse_term};
