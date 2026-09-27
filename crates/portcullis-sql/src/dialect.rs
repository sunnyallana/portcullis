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

    /// Row-limiting clause.
    fn limit_clause(&self, rows: u32) -> String {
        format!("LIMIT {rows}")
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
