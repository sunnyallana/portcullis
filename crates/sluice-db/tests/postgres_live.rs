//! Integration tests against a real PostgreSQL server.
//!
//! These are skipped unless `SLUICE_TEST_DATABASE_URL` is set, so the ordinary
//! `cargo test` needs nothing installed. To run them:
//!
//! ```sh
//! docker run -d --name sluice-pg -e POSTGRES_PASSWORD=sluice-test \
//!            -e POSTGRES_DB=sluice -p 55432:5432 postgres:18
//! psql "postgres://postgres:sluice-test@localhost:55432/sluice" \
//!      -f examples/postgres-schema.sql
//! SLUICE_TEST_DATABASE_URL="postgres://postgres:sluice-test@localhost:55432/sluice" \
//!      cargo test -p sluice-db --test postgres_live
//! ```
//!
//! What they cover is the half that unit tests cannot: that the SQL the builder
//! emits is SQL this server accepts, that `information_schema` is read the way
//! we think, and that values survive the driver round trip with their types
//! and precision intact.

#![cfg(feature = "postgres")]

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use sluice_core::spec::OrderTerm;
use sluice_core::{Caller, DataType, Value, WriteMode};
use sluice_db::Backend;
use sluice_db::plan::{ExecCtx, ReadPlan, WritePlan};
use sluice_db::postgres::{PgConfig, PostgresBackend};
use sluice_sql::{Expr, Term};

/// The connection string, or `None` when these tests should not run.
fn dsn() -> Option<String> {
    std::env::var("SLUICE_TEST_DATABASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Connect, or bail out of the test quietly when no server is configured.
macro_rules! backend {
    () => {{
        let Some(dsn) = dsn() else {
            eprintln!("skipped: set SLUICE_TEST_DATABASE_URL to run the live PostgreSQL tests");
            return;
        };
        PostgresBackend::connect(&PgConfig {
            dsn,
            max_connections: 4,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
            schemas: vec!["public".into()],
            statement_timeout: Duration::from_secs(30),
        })
        .await
        .expect("should connect to the test server")
    }};
}

fn caller(region: &str) -> Caller {
    Caller::new("alice", "support_eu").with("region", Value::Text(region.into()))
}

fn ctx<'a>(args: &'a BTreeMap<String, Value>, caller: &'a Caller) -> ExecCtx<'a> {
    ExecCtx {
        args,
        caller,
        timeout: Duration::from_secs(10),
    }
}

fn args(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[tokio::test]
async fn the_schema_is_read_the_way_we_think_it_is() {
    let b = backend!();
    let schema = b.schema().await.unwrap();

    let orders = schema.table("orders").expect("orders should be visible");
    assert_eq!(orders.name, "public.orders");
    assert_eq!(orders.primary_key, vec!["order_no".to_string()]);

    assert_eq!(orders.column("order_no").unwrap().ty, DataType::Text);
    assert_eq!(orders.column("total").unwrap().ty, DataType::Decimal);
    assert_eq!(orders.column("placed_at").unwrap().ty, DataType::Timestamp);
    assert!(!orders.column("order_no").unwrap().nullable);
    assert!(orders.column("card_last4").unwrap().nullable);

    // text[] is not modelled, so it is absent rather than guessed at.
    assert!(
        orders.column("tags").is_none(),
        "an unmodelled column must not appear in the schema"
    );

    let refunds = schema.table("refunds").unwrap();
    assert_eq!(refunds.column("refund_id").unwrap().ty, DataType::Uuid);
    assert!(
        refunds.column("seq").unwrap().generated,
        "an identity column should be marked generated"
    );
}

#[tokio::test]
async fn a_read_binds_its_values_and_decodes_every_type() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let a = args(&[("order_no", Value::Text("8812".into()))]);
    let c = caller("EU");
    let columns = vec![
        "order_no".to_string(),
        "status".to_string(),
        "placed_at".to_string(),
        "total".to_string(),
        "card_last4".to_string(),
    ];

    let rows = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: Some(&row_filter),
                order_by: &[],
                limit: 10,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let row = &rows.rows[0];
    assert_eq!(row[0], Value::Text("8812".into()));
    // numeric(12,2) keeps its scale rather than becoming a float.
    assert_eq!(row[3], Value::Decimal("1200.00".parse().unwrap()));
    assert!(matches!(row[2], Value::Timestamp(_)));
    assert_eq!(rows.to_json()[0]["total"], "1200.00");
}

#[tokio::test]
async fn the_row_filter_hides_other_regions_on_a_real_server() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let a = args(&[("order_no", Value::Text("8813".into()))]); // a US order
    let columns = vec!["order_no".to_string()];

    let eu = caller("EU");
    let hidden = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: Some(&row_filter),
                order_by: &[],
                limit: 10,
            },
            &ctx(&a, &eu),
        )
        .await
        .unwrap();
    assert_eq!(hidden.len(), 0);

    let us = caller("US");
    let visible = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: Some(&row_filter),
                order_by: &[],
                limit: 10,
            },
            &ctx(&a, &us),
        )
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
}

#[tokio::test]
async fn sql_in_an_argument_reaches_the_server_as_a_bound_value() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let columns = vec!["order_no".to_string()];
    let c = caller("EU");

    for payload in [
        "8812' OR '1'='1",
        "8812'; DROP TABLE orders; --",
        "' UNION SELECT order_no FROM orders --",
    ] {
        let a = args(&[("order_no", Value::Text(payload.into()))]);
        let rows = b
            .read(
                &ReadPlan {
                    table: "orders",
                    columns: &columns,
                    filter: Some(&filter),
                    row_filter: None,
                    order_by: &[],
                    limit: 10,
                },
                &ctx(&a, &c),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 0, "`{payload}` must match nothing");
    }

    // The table survived all of that.
    let a = args(&[("order_no", Value::Text("8812".into()))]);
    let rows = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: None,
                order_by: &[],
                limit: 10,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn ordering_limits_and_optional_parameters_work_against_the_server() {
    let b = backend!();
    let filter = Expr::parse("status in ('open', 'held') and total >= :min_total").unwrap();
    let order_by = [OrderTerm {
        column: "placed_at".into(),
        descending: true,
    }];
    let columns = vec!["order_no".to_string(), "placed_at".to_string()];
    let c = caller("EU");

    // Without the optional parameter its predicate drops out entirely.
    let a = args(&[]);
    let all = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: None,
                order_by: &order_by,
                limit: 10,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();
    assert!(all.len() >= 3);
    // Newest first. No region filter here, so the APAC order placed on the
    // 25th leads.
    assert_eq!(all.rows[0][0], Value::Text("8816".into()));

    let a = args(&[("min_total", Value::Decimal("300.00".parse().unwrap()))]);
    let filtered = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: None,
                order_by: &order_by,
                limit: 10,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();
    assert!(filtered.len() < all.len());

    // The limit is the server's, not ours.
    let a = args(&[]);
    let capped = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: Some(&filter),
                row_filter: None,
                order_by: &order_by,
                limit: 1,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();
    assert_eq!(capped.len(), 1);
}

#[tokio::test]
async fn an_insert_writes_the_row_filter_into_a_not_null_column() {
    let b = backend!();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let mut columns = IndexMap::new();
    columns.insert("refund_id".to_string(), Term::NewUuid);
    columns.insert("order_no".to_string(), Term::Param("order_no".into()));
    columns.insert("amount".to_string(), Term::Param("amount".into()));
    columns.insert(
        "reason".to_string(),
        Term::Lit(Value::Text("live test".into())),
    );
    columns.insert("issued_by".to_string(), Term::Caller("id".into()));
    columns.insert("issued_at".to_string(), Term::Now);

    // `refunds.region` is NOT NULL and the action never sets it; the row
    // filter has to supply it or the insert fails outright.
    let a = args(&[
        ("order_no", Value::Text("8814".into())),
        ("amount", Value::Decimal("12.34".parse().unwrap())),
    ]);
    let c = caller("EU");
    let returning = vec![
        "refund_id".to_string(),
        "region".to_string(),
        "amount".to_string(),
    ];

    let outcome = b
        .write(
            &WritePlan {
                table: "refunds",
                mode: WriteMode::Insert,
                columns: &columns,
                keys: &[],
                returning: &returning,
                row_filter: Some(&row_filter),
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();

    assert_eq!(outcome.rows_affected, 1);
    let row = &outcome.returned.rows[0];
    assert!(
        matches!(row[0], Value::Uuid(_)),
        "RETURNING should decode a uuid"
    );
    assert_eq!(
        row[1],
        Value::Text("EU".into()),
        "the caller's scope, not the action's"
    );
    assert_eq!(row[2], Value::Decimal("12.34".parse().unwrap()));
}

#[tokio::test]
async fn an_update_applies_the_row_filter_in_the_where_clause() {
    let b = backend!();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let mut columns = IndexMap::new();
    columns.insert("order_no".to_string(), Term::Param("order_no".into()));
    columns.insert("status".to_string(), Term::Param("status".into()));
    let a = args(&[
        ("order_no", Value::Text("8812".into())),
        ("status", Value::Text("open".into())),
    ]);

    // A US caller may not touch an EU row, even naming its key exactly.
    let us = caller("US");
    let blocked = b
        .write(
            &WritePlan {
                table: "orders",
                mode: WriteMode::Update,
                columns: &columns,
                keys: &["order_no".to_string()],
                returning: &[],
                row_filter: Some(&row_filter),
            },
            &ctx(&a, &us),
        )
        .await
        .unwrap();
    assert_eq!(
        blocked.rows_affected, 0,
        "the WHERE clause should exclude it"
    );

    let eu = caller("EU");
    let allowed = b
        .write(
            &WritePlan {
                table: "orders",
                mode: WriteMode::Update,
                columns: &columns,
                keys: &["order_no".to_string()],
                returning: &[],
                row_filter: Some(&row_filter),
            },
            &ctx(&a, &eu),
        )
        .await
        .unwrap();
    assert_eq!(allowed.rows_affected, 1);
}

#[tokio::test]
async fn a_statement_that_the_server_rejects_comes_back_as_a_backend_error() {
    let b = backend!();
    let mut columns = IndexMap::new();
    columns.insert("refund_id".to_string(), Term::NewUuid);
    // order_no violates the foreign key to orders.
    columns.insert(
        "order_no".to_string(),
        Term::Lit(Value::Text("does-not-exist".into())),
    );
    columns.insert("region".to_string(), Term::Lit(Value::Text("EU".into())));
    columns.insert(
        "amount".to_string(),
        Term::Lit(Value::Decimal("1.00".parse().unwrap())),
    );
    columns.insert(
        "reason".to_string(),
        Term::Lit(Value::Text("fk probe".into())),
    );
    columns.insert(
        "issued_by".to_string(),
        Term::Lit(Value::Text("test".into())),
    );
    columns.insert("issued_at".to_string(), Term::Now);

    let a = args(&[]);
    let c = caller("EU");
    let err = b
        .write(
            &WritePlan {
                table: "refunds",
                mode: WriteMode::Insert,
                columns: &columns,
                keys: &[],
                returning: &[],
                row_filter: None,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap_err();

    let text = format!("{err}");
    assert!(text.contains("database rejected the statement"), "{text}");
    // The SQLSTATE is carried through so an operator can look it up.
    assert!(
        text.contains("23503"),
        "expected a foreign key violation code: {text}"
    );
}

#[tokio::test]
async fn health_check_answers() {
    let b = backend!();
    b.health().await.unwrap();
    assert!(b.describe().starts_with("postgres://"));
    assert!(
        !b.describe().contains("sluice-test"),
        "the password must not be in the label"
    );
}
