//! Per-database SQL spelling.
//!
//! Only identifier quoting, placeholder syntax and the upsert form differ
//! between the engines Portcullis targets. Everything else the builder emits is
//! plain SQL-92.

use std::fmt;

use portcullis_core::{Error, Result};

/// The SQL spelling of one database engine.
pub trait Dialect: fmt::Debug + Send + Sync {
    /// Name used in diagnostics.
    fn name(&self) -> &'static str;

    /// Quote one already-validated identifier part.
    fn quote_part(&self, part: &str) -> String;

    /// Placeholder for the nth bind parameter, counting from 1.
    fn placeholder(&self, index: usize) -> String;

    /// The `ON CONFLICT` / `ON DUPLICATE KEY` clause for an upsert.
    fn upsert_clause(&self, keys: &[String], updates: &[String]) -> Result<String>;

    /// Row-limiting clause, appended after `ORDER BY`.
    fn limit_clause(&self, rows: u32) -> String {
        format!("LIMIT {rows}")
    }

    /// Row-limiting text that goes straight after `SELECT` instead.
    ///
    /// T-SQL puts the limit at the front as `TOP (n)`, and its `OFFSET/FETCH`
    /// form needs an `ORDER BY` that an action may not have. Dialects that
    /// limit at the end return nothing here.
    fn row_limit_prefix(&self, _rows: u32) -> String {
        String::new()
    }

    /// Whether `INSERT ... RETURNING` is available.
    fn supports_returning(&self) -> bool {
        true
    }

    /// Quote a possibly dotted identifier such as `public.orders`.
    ///
    /// Identifiers reach this point only after being matched against the live
    /// schema, but the check is repeated here because this is the one function
    /// that puts a name directly into SQL text.
    fn quote(&self, ident: &str) -> Result<String> {
        let mut parts = Vec::new();
        for part in ident.split('.') {
            if part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(Error::Backend(format!(
                    "identifier `{ident}` contains characters that cannot be placed in SQL"
                )));
            }
            parts.push(self.quote_part(part));
        }
        Ok(parts.join("."))
    }
}

/// PostgreSQL, and anything that speaks its wire protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct Postgres;

impl Dialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn quote_part(&self, part: &str) -> String {
        format!("\"{part}\"")
    }

    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }

    fn upsert_clause(&self, keys: &[String], updates: &[String]) -> Result<String> {
        if keys.is_empty() {
            return Err(Error::Backend("an upsert needs key columns".into()));
        }
        let conflict = keys
            .iter()
            .map(|k| self.quote(k))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        if updates.is_empty() {
            return Ok(format!("ON CONFLICT ({conflict}) DO NOTHING"));
        }
        let sets = updates
            .iter()
            .map(|c| {
                let q = self.quote(c)?;
                Ok(format!("{q} = EXCLUDED.{q}"))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        Ok(format!("ON CONFLICT ({conflict}) DO UPDATE SET {sets}"))
    }
}

/// MySQL and MariaDB.
#[derive(Debug, Clone, Copy, Default)]
pub struct MySql;

impl Dialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn quote_part(&self, part: &str) -> String {
        format!("`{part}`")
    }

    fn placeholder(&self, _index: usize) -> String {
        // MySQL binds positionally, in the order the placeholders appear.
        "?".to_owned()
    }

    fn upsert_clause(&self, keys: &[String], updates: &[String]) -> Result<String> {
        if keys.is_empty() {
            return Err(Error::Backend("an upsert needs key columns".into()));
        }
        if updates.is_empty() {
            // MySQL has no DO NOTHING; assigning a key to itself is the
            // idiomatic equivalent and touches nothing.
            let first = self.quote(&keys[0])?;
            return Ok(format!("ON DUPLICATE KEY UPDATE {first} = {first}"));
        }
        let sets = updates
            .iter()
            .map(|c| {
                let q = self.quote(c)?;
                Ok(format!("{q} = VALUES({q})"))
            })
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        Ok(format!("ON DUPLICATE KEY UPDATE {sets}"))
    }

    fn supports_returning(&self) -> bool {
        false
    }
}

/// Microsoft SQL Server.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlServer;

impl Dialect for SqlServer {
    fn name(&self) -> &'static str {
        "sqlserver"
    }

    fn quote_part(&self, part: &str) -> String {
        format!("[{part}]")
    }

    fn placeholder(&self, index: usize) -> String {
        format!("@P{index}")
    }

    fn upsert_clause(&self, _keys: &[String], _updates: &[String]) -> Result<String> {
        // T-SQL spells this `MERGE`, which is a different statement shape
        // rather than a suffix on an insert, and a naive
        // `IF EXISTS … UPDATE ELSE INSERT` races. Refused rather than
        // implemented badly: an upsert that sometimes writes twice is worse
        // than one that is not offered.
        Err(Error::Backend(
            "SQL Server upserts are not implemented; use mode = \"insert\" or \"update\"".into(),
        ))
    }

    fn limit_clause(&self, _rows: u32) -> String {
        String::new()
    }

    fn row_limit_prefix(&self, rows: u32) -> String {
        format!("TOP ({rows}) ")
    }

    fn supports_returning(&self) -> bool {
        // It has OUTPUT INSERTED, but that sits mid-statement rather than at
        // the end, so the backend reads the row back instead.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_identifiers_are_quoted_part_by_part() {
        assert_eq!(
            Postgres.quote("public.orders").unwrap(),
            "\"public\".\"orders\""
        );
    }

    #[test]
    fn identifiers_with_punctuation_are_refused() {
        for bad in ["orders\"; drop table x --", "orders-1", "", "a..b"] {
            assert!(Postgres.quote(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn sqlserver_brackets_identifiers_and_numbers_its_placeholders() {
        assert_eq!(SqlServer.quote("dbo.orders").unwrap(), "[dbo].[orders]");
        assert_eq!(SqlServer.placeholder(1), "@P1");
        assert_eq!(SqlServer.placeholder(12), "@P12");
        assert!(SqlServer.quote("orders]; DROP TABLE x --").is_err());
    }

    #[test]
    fn sqlserver_limits_at_the_front_not_the_end() {
        assert_eq!(SqlServer.row_limit_prefix(50), "TOP (50) ");
        assert_eq!(SqlServer.limit_clause(50), "");
        assert_eq!(Postgres.row_limit_prefix(50), "");
        assert_eq!(Postgres.limit_clause(50), "LIMIT 50");
    }

    #[test]
    fn sqlserver_refuses_an_upsert_rather_than_racing() {
        let err = SqlServer
            .upsert_clause(&["id".into()], &["note".into()])
            .unwrap_err();
        assert!(format!("{err}").contains("not implemented"), "{err}");
    }

    #[test]
    fn mysql_quotes_with_backticks_and_binds_positionally() {
        assert_eq!(MySql.quote("app.orders").unwrap(), "`app`.`orders`");
        assert_eq!(MySql.placeholder(1), "?");
        assert_eq!(MySql.placeholder(7), "?");
        assert!(MySql.quote("orders`; DROP TABLE x").is_err());
    }

    #[test]
    fn mysql_upsert_uses_on_duplicate_key() {
        let clause = MySql
            .upsert_clause(&["id".into()], &["amount".into()])
            .unwrap();
        assert_eq!(
            clause,
            "ON DUPLICATE KEY UPDATE `amount` = VALUES(`amount`)"
        );
        // No columns to update is expressed as a self-assignment.
        let nothing = MySql.upsert_clause(&["id".into()], &[]).unwrap();
        assert_eq!(nothing, "ON DUPLICATE KEY UPDATE `id` = `id`");
    }

    #[test]
    fn upsert_updates_every_non_key_column() {
        let clause = Postgres
            .upsert_clause(&["id".into()], &["amount".into(), "reason".into()])
            .unwrap();
        assert_eq!(
            clause,
            "ON CONFLICT (\"id\") DO UPDATE SET \"amount\" = EXCLUDED.\"amount\", \"reason\" = EXCLUDED.\"reason\""
        );
    }
}
