//! Integration tests against a real MySQL server.
//!
//! Skipped unless `PORTCULLIS_TEST_MYSQL_URL` is set. To run them:
//!
//! ```sh
//! docker run -d --name portcullis-mysql -e MYSQL_ROOT_PASSWORD=portcullis-test \
//!            -e MYSQL_DATABASE=portcullis -p 33306:3306 mysql:9
//! mysql -h127.0.0.1 -P33306 -uroot -pportcullis-test portcullis < examples/mysql-schema.sql
//! PORTCULLIS_TEST_MYSQL_URL="mysql://root:portcullis-test@localhost:33306/portcullis" \
//!      cargo test -p portcullis-db --features mysql --test mysql_live
//! ```
//!
//! The interesting case here is the one PostgreSQL does not have: MySQL cannot
//! return columns from a write, so the backend finds the row again. These
//! tests check that it finds the right one.

#![cfg(feature = "mysql")]

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use portcullis_core::{Caller, DataType, Value, WriteMode};
use portcullis_db::Backend;
use portcullis_db::mysql::{MySqlBackend, MySqlConfig};
use portcullis_db::plan::{ExecCtx, ReadPlan, WritePlan};
use portcullis_sql::{Expr, Term};

fn dsn() -> Option<String> {
    std::env::var("PORTCULLIS_TEST_MYSQL_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

macro_rules! backend {
    () => {{
        let Some(dsn) = dsn() else {
            eprintln!("skipped: set PORTCULLIS_TEST_MYSQL_URL to run the live MySQL tests");
            return;
        };
        MySqlBackend::connect(&MySqlConfig {
            dsn,
            max_connections: 4,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
            schemas: Vec::new(),
            statement_timeout: Duration::from_secs(30),
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
    assert_eq!(orders.primary_key, vec!["order_no".to_string()]);
    assert_eq!(orders.column("total").unwrap().ty, DataType::Decimal);
    assert_eq!(orders.column("placed_at").unwrap().ty, DataType::Timestamp);
    assert!(!orders.column("order_no").unwrap().nullable);
    assert!(orders.column("card_last4").unwrap().nullable);
    assert!(
        orders.column("attachment").is_none(),
        "a BLOB is not modelled, so it must not appear"
    );
}

#[tokio::test]
async fn reads_bind_positionally_and_decode() {
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
    assert_eq!(hidden.len(), 0);
}

#[tokio::test]
async fn sql_in_an_argument_is_bound_not_interpreted() {
    let b = backend!();
    let filter = Expr::parse("order_no = :order_no").unwrap();
    let columns = vec!["order_no".to_string()];
    let c = caller("EU");

    for payload in ["8812' OR '1'='1", "8812'; DROP TABLE orders; --"] {
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
async fn an_insert_returns_its_row_even_without_returning_support() {
    let b = backend!();
    let row_filter = Expr::parse("region = $caller.region").unwrap();
    let id = uuid::Uuid::new_v4().to_string();

    let mut columns = IndexMap::new();
    columns.insert("refund_id".to_string(), Term::Param("refund_id".into()));
    columns.insert("order_no".to_string(), Term::Param("order_no".into()));
    columns.insert("amount".to_string(), Term::Param("amount".into()));
    columns.insert(
        "reason".to_string(),
        Term::Lit(Value::Text("mysql live test".into())),
    );
    columns.insert("issued_by".to_string(), Term::Caller("id".into()));
    columns.insert("issued_at".to_string(), Term::Now);

    let a = args(&[
        ("refund_id", Value::Text(id.clone())),
        ("order_no", Value::Text("8814".into())),
        ("amount", Value::Decimal("21.50".parse().unwrap())),
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
    assert_eq!(row[0].to_string(), id, "the right row, not just any row");
    assert_eq!(
        row[1],
        Value::Text("EU".into()),
        "the row filter wrote the caller's region"
    );
    assert_eq!(row[2], Value::Decimal("21.50".parse().unwrap()));
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
    // MySQL reports 0 affected when the new values equal the old ones, so the
    // test only asserts it was not refused.
    assert!(allowed.rows_affected <= 1);
}

#[tokio::test]
async fn an_upsert_updates_the_existing_row() {
    let b = backend!();
    let id = uuid::Uuid::new_v4().to_string();
    let mut columns = IndexMap::new();
    columns.insert("refund_id".to_string(), Term::Param("refund_id".into()));
    columns.insert("order_no".to_string(), Term::Param("order_no".into()));
    columns.insert("region".to_string(), Term::Lit(Value::Text("EU".into())));
    columns.insert("amount".to_string(), Term::Param("amount".into()));
    columns.insert("reason".to_string(), Term::Param("reason".into()));
    columns.insert("issued_by".to_string(), Term::Caller("id".into()));
    columns.insert("issued_at".to_string(), Term::Now);
    let keys = vec!["refund_id".to_string()];
    let returning = vec!["reason".to_string()];

    let first = args(&[
        ("refund_id", Value::Text(id.clone())),
        ("order_no", Value::Text("8814".into())),
        ("amount", Value::Decimal("5.00".parse().unwrap())),
        ("reason", Value::Text("first".into())),
    ]);
    let c = caller("EU");
    let plan = || WritePlan {
        table: "refunds",
        mode: WriteMode::Upsert,
        columns: &columns,
        keys: &keys,
        returning: &returning,
        row_filter: None,
    };
    let out = b.write(&plan(), &ctx(&first, &c)).await.unwrap();
    assert_eq!(out.returned.rows[0][0], Value::Text("first".into()));

    let second = args(&[
        ("refund_id", Value::Text(id)),
        ("order_no", Value::Text("8814".into())),
        ("amount", Value::Decimal("5.00".parse().unwrap())),
        ("reason", Value::Text("second".into())),
    ]);
    let out = b.write(&plan(), &ctx(&second, &c)).await.unwrap();
    assert_eq!(
        out.returned.rows[0][0],
        Value::Text("second".into()),
        "the upsert replaced the reason on the existing row"
    );
}

#[tokio::test]
async fn a_rejected_statement_carries_the_server_code() {
    let b = backend!();
    let mut columns = IndexMap::new();
    columns.insert(
        "refund_id".to_string(),
        Term::Lit(Value::Text(uuid::Uuid::new_v4().to_string())),
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
    assert!(text.contains("1452") || text.contains("23000"), "{text}");
}

#[tokio::test]
async fn health_check_answers_and_the_label_hides_the_password() {
    let b = backend!();
    b.health().await.unwrap();
    assert!(b.describe().starts_with("mysql://"));
    assert!(!b.describe().contains("portcullis-test"));
}

#[tokio::test]
async fn shared_limits_are_enforced_across_processes() {
    let b = backend!();
    assert!(
        b.supports_shared_state().await.unwrap(),
        "the shared-state tables should exist; load examples/shared-state-mysql.sql"
    );

    // A caller unique to this run, so the test does not collide with itself.
    let caller = format!("rate-{}", uuid::Uuid::new_v4());
    for i in 1..=3u32 {
        assert!(
            b.rate_check(&caller, "find_order", 3).await.unwrap(),
            "call {i} of 3 should be inside the limit"
        );
    }
    assert!(
        !b.rate_check(&caller, "find_order", 3).await.unwrap(),
        "the fourth call is over the limit"
    );
    // A different action has its own counter.
    assert!(b.rate_check(&caller, "list_orders", 3).await.unwrap());
}

#[tokio::test]
async fn replay_protection_survives_in_the_database() {
    let b = backend!();
    let key = uuid::Uuid::new_v4().simple().to_string();
    let ttl = Duration::from_secs(3600);

    assert!(b.replay_get(&key, ttl).await.unwrap().is_none());

    let first = serde_json::json!({ "refund_id": "abc", "amount": "12.34" });
    b.replay_put(&key, "refund_order", &first, ttl)
        .await
        .unwrap();
    assert_eq!(b.replay_get(&key, ttl).await.unwrap(), Some(first.clone()));

    // A second write under the same key keeps the first answer: the caller
    // already has it, and a retry must keep getting the same one.
    let second = serde_json::json!({ "refund_id": "different" });
    b.replay_put(&key, "refund_order", &second, ttl)
        .await
        .unwrap();
    assert_eq!(b.replay_get(&key, ttl).await.unwrap(), Some(first));

    // Past the window it is gone.
    assert!(b.replay_get(&key, Duration::ZERO).await.unwrap().is_none());
}
