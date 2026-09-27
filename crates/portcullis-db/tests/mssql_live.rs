//! Integration tests against a real SQL Server.
//!
//! Skipped unless `PORTCULLIS_TEST_MSSQL_DSN` is set. To run them:
//!
//! ```sh
//! docker run -d --name portcullis-mssql -e ACCEPT_EULA=Y \
//!   -e MSSQL_SA_PASSWORD=Portcullis-test1 -e MSSQL_PID=Developer \
//!   -p 21433:1433 mcr.microsoft.com/mssql/server:2022-latest
//! sqlcmd -S localhost,21433 -U sa -P Portcullis-test1 -C \
//!   -Q "CREATE DATABASE portcullis"
//! sqlcmd -S localhost,21433 -U sa -P Portcullis-test1 -C -d portcullis \
//!   -i examples/sqlserver-schema.sql
//! PORTCULLIS_TEST_MSSQL_DSN="Server=tcp:localhost,21433;User Id=sa;Password=Portcullis-test1;Database=portcullis;TrustServerCertificate=true" \
//!   cargo test -p portcullis-db --features mssql --test mssql_live
//! ```
//!
//! What matters here is the three places T-SQL differs: `@P1` placeholders,
//! `TOP (n)` at the front instead of `LIMIT` at the end, and no `RETURNING`.

#![cfg(feature = "mssql")]

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use portcullis_core::{Caller, DataType, Value, WriteMode};
use portcullis_db::Backend;
use portcullis_db::mssql::{MsSqlBackend, MsSqlConfig};
use portcullis_db::plan::{ExecCtx, ReadPlan, WritePlan};
use portcullis_sql::{Expr, Term};

fn dsn() -> Option<String> {
    std::env::var("PORTCULLIS_TEST_MSSQL_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

macro_rules! backend {
    () => {{
        let Some(dsn) = dsn() else {
            eprintln!("skipped: set PORTCULLIS_TEST_MSSQL_DSN to run the live SQL Server tests");
            return;
        };
        MsSqlBackend::connect(&MsSqlConfig {
            dsn,
            max_connections: 4,
            acquire_timeout: Duration::from_secs(10),
            schemas: vec!["dbo".into()],
        })
        .await
        .expect("should connect to the test server")
    }};
}

fn caller(region: &str) -> Caller {
    Caller::new("alice", "support").with("region", Value::Text(region.into()))
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
async fn the_catalogue_is_read_the_way_we_think_it_is() {
    let b = backend!();
    let schema = b.schema().await.unwrap();

    let orders = schema.table("orders").expect("orders should be visible");
    assert_eq!(orders.name, "dbo.orders");
    assert_eq!(orders.primary_key, vec!["order_no".to_string()]);
    assert_eq!(orders.column("total").unwrap().ty, DataType::Decimal);
    assert_eq!(orders.column("placed_at").unwrap().ty, DataType::Timestamp);
    assert!(!orders.column("order_no").unwrap().nullable);
    assert!(orders.column("card_last4").unwrap().nullable);
    assert!(
        orders.column("attachment").is_none(),
        "varbinary is not modelled, so it must not appear"
    );

    let refunds = schema.table("refunds").unwrap();
    assert_eq!(refunds.column("refund_id").unwrap().ty, DataType::Uuid);
    assert!(
        refunds.column("seq").unwrap().generated,
        "an IDENTITY column should be marked generated"
    );
}

#[tokio::test]
async fn reads_bind_as_named_parameters_and_decode() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let a = args(&[("order_no", Value::Text("8812".into()))]);
    let c = caller("EU");
    let columns = vec![
        "order_no".to_string(),
        "total".to_string(),
        "placed_at".to_string(),
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
    assert_eq!(rows.rows[0][1], Value::Decimal("1200.00".parse().unwrap()));
    assert!(matches!(rows.rows[0][2], Value::Timestamp(_)));
}

#[tokio::test]
async fn the_row_limit_is_applied_at_the_front() {
    let b = backend!();
    let columns = vec!["order_no".to_string()];
    let a = args(&[]);
    let c = caller("EU");

    // TOP (n) rather than LIMIT n, and without an ORDER BY, which is the
    // case OFFSET/FETCH could not have covered.
    let rows = b
        .read(
            &ReadPlan {
                table: "orders",
                columns: &columns,
                filter: None,
                row_filter: None,
                order_by: &[],
                limit: 2,
            },
            &ctx(&a, &c),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn the_row_filter_hides_other_regions() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let a = args(&[("order_no", Value::Text("8813".into()))]);
    let columns = vec!["order_no".to_string()];

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
            &ctx(&a, &caller("EU")),
        )
        .await
        .unwrap();
    assert_eq!(hidden.len(), 0, "8813 is a US order");
}

#[tokio::test]
async fn sql_in_an_argument_is_bound_not_interpreted() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let columns = vec!["order_no".to_string()];
    let c = caller("EU");

    for payload in ["8812' OR '1'='1", "8812'; DROP TABLE dbo.orders; --"] {
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
    assert_eq!(rows.len(), 1, "the table survived");
}

#[tokio::test]
async fn an_insert_returns_its_row_without_returning_support() {
    let b = backend!();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let id = uuid::Uuid::new_v4();

    let mut columns = IndexMap::new();
    columns.insert("refund_id".to_string(), Term::Param("refund_id".into()));
    columns.insert("order_no".to_string(), Term::Param("order_no".into()));
    columns.insert("amount".to_string(), Term::Param("amount".into()));
    columns.insert(
        "reason".to_string(),
        Term::Lit(Value::Text("sqlserver live test".into())),
    );
    columns.insert("issued_by".to_string(), Term::Caller("id".into()));
    columns.insert("issued_at".to_string(), Term::Now);

    let a = args(&[
        ("refund_id", Value::Uuid(id)),
        ("order_no", Value::Text("8814".into())),
        ("amount", Value::Decimal("33.25".parse().unwrap())),
    ]);
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
            &ctx(&a, &caller("EU")),
        )
        .await
        .unwrap();

    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.returned.len(), 1, "the row was found again");
    let row = &outcome.returned.rows[0];
    assert_eq!(row[0], Value::Uuid(id), "the right row, not just any row");
    assert_eq!(
        row[1],
        Value::Text("EU".into()),
        "the row filter wrote the caller's region"
    );
    assert_eq!(row[2], Value::Decimal("33.25".parse().unwrap()));
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
            &ctx(&a, &caller("US")),
        )
        .await
        .unwrap();
    assert_eq!(
        blocked.rows_affected, 0,
        "a US caller cannot touch an EU row"
    );

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
            &ctx(&a, &caller("EU")),
        )
        .await
        .unwrap();
    assert_eq!(allowed.rows_affected, 1);
}

#[tokio::test]
async fn an_upsert_is_refused_rather_than_raced() {
    let b = backend!();
    let mut columns = IndexMap::new();
    columns.insert("refund_id".to_string(), Term::Param("refund_id".into()));
    let a = args(&[("refund_id", Value::Uuid(uuid::Uuid::new_v4()))]);

    let err = b
        .write(
            &WritePlan {
                table: "refunds",
                mode: WriteMode::Upsert,
                columns: &columns,
                keys: &["refund_id".to_string()],
                returning: &[],
                row_filter: None,
            },
            &ctx(&a, &caller("EU")),
        )
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("not implemented"), "{err}");
}

#[tokio::test]
async fn a_rejected_statement_carries_the_server_code() {
    let b = backend!();
    let mut columns = IndexMap::new();
    columns.insert(
        "refund_id".to_string(),
        Term::Lit(Value::Uuid(uuid::Uuid::new_v4())),
    );
    columns.insert(
        "order_no".to_string(),
        Term::Lit(Value::Text("does-not-exist".into())),
    );
    columns.insert("region".to_string(), Term::Lit(Value::Text("EU".into())));
    columns.insert(
        "amount".to_string(),
        Term::Lit(Value::Decimal("1.00".parse().unwrap())),
    );
    columns.insert("reason".to_string(), Term::Lit(Value::Text("fk".into())));
    columns.insert("issued_by".to_string(), Term::Lit(Value::Text("t".into())));
    columns.insert("issued_at".to_string(), Term::Now);

    let a = args(&[]);
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
            &ctx(&a, &caller("EU")),
        )
        .await
        .unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("database rejected the statement"), "{text}");
    // 547 is a foreign key violation.
    assert!(text.contains("547"), "{text}");
}

#[tokio::test]
async fn health_answers_and_the_label_hides_the_password() {
    let b = backend!();
    b.health().await.unwrap();
    assert!(b.describe().starts_with("sqlserver:"));
    assert!(
        !b.describe().contains("Portcullis-test1"),
        "{}",
        b.describe()
    );
}
