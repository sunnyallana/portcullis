//! A description of the live database, read from the server at startup.
//!
//! Portcullis never trusts an action definition on its own. Every table, column and
//! parameter an operator writes is checked against this structure before the
//! action is published, so a typo fails at boot rather than at 3am inside an
//! agent's tool call.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::value::DataType;

/// One column of one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// Column name as the database reports it.
    pub name: String,
    /// Type, narrowed to Portcullis's value model.
    #[serde(rename = "type")]
    pub ty: DataType,
    /// Whether the database accepts NULL here.
    #[serde(default = "default_true")]
    pub nullable: bool,
    /// True when the database generates the value (identity, serial, default).
    #[serde(default)]
    pub generated: bool,
}

fn default_true() -> bool {
    true
}

/// One table or view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// Schema-qualified name, for example `public.orders`.
    pub name: String,
    /// Columns in declaration order.
    pub columns: Vec<Column>,
    /// Primary key columns, empty when the table has none.
    #[serde(default)]
    pub primary_key: Vec<String>,
}

impl Table {
    /// Find a column by name, case-insensitively.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Column names, for "did you mean" suggestions.
    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }
}

/// Everything Portcullis knows about the target database.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// Tables and views, keyed by their qualified name.
    pub tables: BTreeMap<String, Table>,
}

impl Schema {
    /// Build a schema from a list of tables.
    pub fn new(tables: impl IntoIterator<Item = Table>) -> Self {
        Self {
            tables: tables.into_iter().map(|t| (t.name.clone(), t)).collect(),
        }
    }

    /// Look up a table.
    ///
    /// Accepts an unqualified name when exactly one schema contains it, so that
    /// `orders` resolves to `public.orders` without the operator having to say
    /// so. An ambiguous name returns `None` and the caller reports the conflict.
    pub fn table(&self, name: &str) -> Option<&Table> {
        if let Some(t) = self.tables.get(name) {
            return Some(t);
        }
        let mut hits = self.tables.values().filter(|t| {
            t.name.eq_ignore_ascii_case(name)
                || t.name
                    .rsplit_once('.')
                    .is_some_and(|(_, bare)| bare.eq_ignore_ascii_case(name))
        });
        let first = hits.next()?;
        if hits.next().is_some() {
            None
        } else {
            Some(first)
        }
    }

    /// True when more than one schema holds a table with this bare name.
    pub fn is_ambiguous(&self, name: &str) -> bool {
        !name.contains('.')
            && self
                .tables
                .values()
                .filter(|t| {
                    t.name
                        .rsplit_once('.')
                        .is_some_and(|(_, bare)| bare.eq_ignore_ascii_case(name))
                })
                .count()
                > 1
    }

    /// Table names, for "did you mean" suggestions.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.keys().map(String::as_str).collect()
    }
}

/// Pick the closest candidate to `input`, for a "did you mean" hint.
///
/// Uses a bounded edit distance; anything further than a third of the word is
/// not offered, because a wrong suggestion wastes more time than none.
pub fn did_you_mean<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let limit = (input.len() / 3).max(2);
    candidates
        .iter()
        .map(|c| {
            (
                *c,
                edit_distance(&input.to_ascii_lowercase(), &c.to_ascii_lowercase()),
            )
        })
        .filter(|(_, d)| *d <= limit)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut cur = vec![0usize; b_chars.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        Schema::new([
            Table {
                name: "public.orders".into(),
                columns: vec![Column {
                    name: "order_no".into(),
                    ty: DataType::Text,
                    nullable: false,
                    generated: false,
                }],
                primary_key: vec!["order_no".into()],
            },
            Table {
                name: "public.refunds".into(),
                columns: vec![],
                primary_key: vec![],
            },
        ])
    }

    #[test]
    fn bare_names_resolve_when_unambiguous() {
        assert_eq!(schema().table("orders").unwrap().name, "public.orders");
        assert_eq!(
            schema().table("public.orders").unwrap().name,
            "public.orders"
        );
    }

    #[test]
    fn ambiguous_bare_names_do_not_resolve() {
        let s = Schema::new([
            Table {
                name: "public.orders".into(),
                columns: vec![],
                primary_key: vec![],
            },
            Table {
                name: "archive.orders".into(),
                columns: vec![],
                primary_key: vec![],
            },
        ]);
        assert!(s.table("orders").is_none());
        assert!(s.is_ambiguous("orders"));
    }

    #[test]
    fn suggestions_are_offered_for_near_misses_only() {
        let names = ["order_no", "status", "customer_email"];
        assert_eq!(did_you_mean("order_num", &names), Some("order_no"));
        assert_eq!(did_you_mean("total_price", &names), None);
    }
}
