//! The PostgreSQL backend.
//!
//! Connections come from a pool sized by configuration; statements are built by
//! `sluice-sql` and every value travels as a bind parameter. Results are
//! decoded against the column types read from `information_schema`, so a
//! column whose type Sluice does not model is never guessed at: it is left out
//! of the schema, and any action naming it fails validation at startup.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use sluice_core::{Column, DataType, Error, Result, Schema, Table, Value};
use sluice_sql::{Binder, Postgres as PgDialect, ReadQuery, Statement, WriteQuery};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::{AssertSqlSafe, Pool, Row as _};
use tokio::sync::RwLock;

use crate::Backend;
use crate::plan::{ExecCtx, ReadPlan, Rows, WriteOutcome, WritePlan};

/// How to reach a PostgreSQL server.
#[derive(Debug, Clone)]
pub struct PgConfig {
    /// libpq-style connection string.
    pub dsn: String,
    /// Maximum pooled connections.
    pub max_connections: u32,
    /// Minimum idle connections kept warm.
    pub min_connections: u32,
    /// How long to wait for a connection from the pool.
    pub acquire_timeout: Duration,
    /// Schemas to expose. Empty means every non-system schema.
    pub schemas: Vec<String>,
    /// Server-side backstop, applied to every connection.
    pub statement_timeout: Duration,
}

impl Default for PgConfig {
    fn default() -> Self {
        Self {
            dsn: String::new(),
            max_connections: 10,
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
            schemas: Vec::new(),
            statement_timeout: Duration::from_secs(60),
        }
    }
}

/// A pooled PostgreSQL backend.
#[derive(Debug)]
pub struct PostgresBackend {
    pool: Pool<sqlx::Postgres>,
    dialect: PgDialect,
    schema: RwLock<Schema>,
    label: String,
    schemas: Vec<String>,
}

impl PostgresBackend {
    /// Connect, verify the server answers and read the schema once.
    pub async fn connect(cfg: &PgConfig) -> Result<Self> {
        let opts: PgConnectOptions = cfg
            .dsn
            .parse()
            .map_err(|e| Error::Config(format!("connection string is not valid: {e}")))?;
        // Rendered without the password, for logs and `sluice doctor`.
        let label = format!(
            "postgres://{}@{}:{}/{}",
            opts.get_username(),
            opts.get_host(),
            opts.get_port(),
            opts.get_database().unwrap_or("postgres")
        );

        let millis = u64::try_from(cfg.statement_timeout.as_millis()).unwrap_or(60_000);
        let pool = PgPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(cfg.acquire_timeout)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    // A backstop in case a client-side timeout is ever missed.
                    sqlx::query(AssertSqlSafe(format!("SET statement_timeout = {millis}")))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(opts)
            .await
            .map_err(|e| Error::Backend(format!("cannot connect to {label}: {e}")))?;

        let backend = Self {
            pool,
            dialect: PgDialect,
            schema: RwLock::new(Schema::default()),
            label,
            schemas: cfg.schemas.clone(),
        };
        let live = backend.read_schema().await?;
        *backend.schema.write().await = live;
        Ok(backend)
    }

    /// Re-read the schema from the server.
    pub async fn refresh_schema(&self) -> Result<Schema> {
        let live = self.read_schema().await?;
        *self.schema.write().await = live.clone();
        Ok(live)
    }

    async fn read_schema(&self) -> Result<Schema> {
        let filter = if self.schemas.is_empty() {
            String::new()
        } else {
            let list = self
                .schemas
                .iter()
                .map(|s| format!("'{}'", s.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" AND c.table_schema IN ({list})")
        };

        let sql = format!(
            "SELECT c.table_schema, c.table_name, c.column_name, c.udt_name, \
                    c.is_nullable, c.is_identity, c.column_default \
             FROM information_schema.columns c \
             JOIN information_schema.tables t \
               ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
             WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') \
               AND t.table_type IN ('BASE TABLE', 'VIEW'){filter} \
             ORDER BY c.table_schema, c.table_name, c.ordinal_position"
        );

        let rows = sqlx::query(AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::Backend(format!("reading the schema failed: {e}")))?;

        let mut tables: BTreeMap<String, Table> = BTreeMap::new();
        for row in rows {
            let schema: String = row.try_get("table_schema").map_err(|e| decode_err(&e))?;
            let name: String = row.try_get("table_name").map_err(|e| decode_err(&e))?;
            let column: String = row.try_get("column_name").map_err(|e| decode_err(&e))?;
            let udt: String = row.try_get("udt_name").map_err(|e| decode_err(&e))?;
            let nullable: String = row.try_get("is_nullable").map_err(|e| decode_err(&e))?;
            let identity: String = row.try_get("is_identity").map_err(|e| decode_err(&e))?;
            let default: Option<String> =
                row.try_get("column_default").map_err(|e| decode_err(&e))?;

            // A column Sluice cannot model is dropped rather than guessed at.
            let Some(ty) = pg_type(&udt) else { continue };

            let qualified = format!("{schema}.{name}");
            tables
                .entry(qualified.clone())
                .or_insert_with(|| Table {
                    name: qualified,
                    columns: Vec::new(),
                    primary_key: Vec::new(),
                })
                .columns
                .push(Column {
                    name: column,
                    ty,
                    nullable: nullable == "YES",
                    generated: identity == "YES" || default.is_some(),
                });
        }

        let pk_sql = "SELECT tc.table_schema, tc.table_name, kcu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
               AND tc.table_schema NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY kcu.ordinal_position";

        for row in sqlx::query(pk_sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::Backend(format!("reading primary keys failed: {e}")))?
        {
            let schema: String = row.try_get("table_schema").map_err(|e| decode_err(&e))?;
            let name: String = row.try_get("table_name").map_err(|e| decode_err(&e))?;
            let column: String = row.try_get("column_name").map_err(|e| decode_err(&e))?;
            if let Some(t) = tables.get_mut(&format!("{schema}.{name}")) {
                t.primary_key.push(column);
            }
        }

        Ok(Schema { tables })
    }

    async fn column_types(&self, table: &str, columns: &[String]) -> Result<Vec<DataType>> {
        let schema = self.schema.read().await;
        let t = schema
            .table(table)
            .ok_or_else(|| Error::Backend(format!("no table named `{table}`")))?;
        columns
            .iter()
            .map(|c| {
                t.column(c)
                    .map(|col| col.ty)
                    .ok_or_else(|| Error::Backend(format!("`{table}` has no column `{c}`")))
            })
            .collect()
    }

    async fn run(
        &self,
        stmt: &Statement,
        types: &[DataType],
        columns: &[String],
        timeout: Duration,
    ) -> Result<Rows> {
        let mut query = sqlx::query(AssertSqlSafe(stmt.sql.as_str()));
        for v in &stmt.binds {
            query = bind(query, v);
        }
        let fut = query.fetch_all(&self.pool);
        let rows = tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| Error::LimitExceeded(format!("statement exceeded {timeout:?}")))?
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        Ok(Rows {
            columns: columns.to_vec(),
            rows: rows
                .iter()
                .map(|r| decode_row(r, types))
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

#[async_trait]
impl Backend for PostgresBackend {
    fn describe(&self) -> String {
        self.label.clone()
    }

    async fn schema(&self) -> Result<Schema> {
        Ok(self.schema.read().await.clone())
    }

    async fn read(&self, plan: &ReadPlan<'_>, ctx: &ExecCtx<'_>) -> Result<Rows> {
        let mut binder = Binder::new(ctx.args, ctx.caller);
        let stmt = sluice_sql::select(
            &ReadQuery {
                table: plan.table,
                columns: plan.columns,
                filter: plan.filter,
                row_filter: plan.row_filter,
                order_by: plan.order_by,
                limit: plan.limit,
            },
            &self.dialect,
            &mut binder,
        )?;
        let types = self.column_types(plan.table, plan.columns).await?;
        self.run(&stmt, &types, plan.columns, ctx.timeout).await
    }

    async fn write(&self, plan: &WritePlan<'_>, ctx: &ExecCtx<'_>) -> Result<WriteOutcome> {
        let mut binder = Binder::new(ctx.args, ctx.caller);
        let stmt = sluice_sql::write(
            &WriteQuery {
                table: plan.table,
                mode: plan.mode,
                columns: plan.columns,
                keys: plan.keys,
                returning: plan.returning,
                row_filter: plan.row_filter,
            },
            &self.dialect,
            &mut binder,
        )?;

        if plan.returning.is_empty() {
            let mut query = sqlx::query(AssertSqlSafe(stmt.sql.as_str()));
            for v in &stmt.binds {
                query = bind(query, v);
            }
            let fut = query.execute(&self.pool);
            let done = tokio::time::timeout(ctx.timeout, fut)
                .await
                .map_err(|_| Error::LimitExceeded(format!("statement exceeded {:?}", ctx.timeout)))?
                .map_err(|e| Error::Backend(sanitise(&e)))?;
            return Ok(WriteOutcome {
                rows_affected: done.rows_affected(),
                returned: Rows::default(),
            });
        }

        let types = self.column_types(plan.table, plan.returning).await?;
        let returned = self.run(&stmt, &types, plan.returning, ctx.timeout).await?;
        Ok(WriteOutcome {
            rows_affected: returned.len() as u64,
            returned,
        })
    }

    async fn health(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Error::Backend(sanitise(&e)))
    }
}

type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

/// Bind one value.
///
/// Nulls never arrive here: the builder writes them into the statement as the
/// `NULL` keyword, because a bind parameter has no correct type for "nothing".
fn bind<'q>(q: PgQuery<'q>, v: &'q Value) -> PgQuery<'q> {
    match v {
        Value::Null => q,
        Value::Bool(b) => q.bind(b),
        Value::Int(i) => q.bind(i),
        Value::Float(f) => q.bind(f),
        Value::Decimal(d) => q.bind(d),
        Value::Text(s) => q.bind(s),
        Value::Timestamp(t) => q.bind(to_chrono(*t)),
        Value::Uuid(u) => q.bind(u),
        Value::Json(j) => q.bind(j),
    }
}

fn decode_row(row: &PgRow, types: &[DataType]) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(types.len());
    for (i, ty) in types.iter().enumerate() {
        out.push(decode(row, i, *ty)?);
    }
    Ok(out)
}

fn decode(row: &PgRow, i: usize, ty: DataType) -> Result<Value> {
    let name = || {
        row.columns()
            .get(i)
            .map_or("?", sqlx::Column::name)
            .to_owned()
    };
    Ok(match ty {
        DataType::Bool => row
            .try_get::<Option<bool>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, Value::Bool),
        DataType::Int => {
            // int2, int4 and int8 all map to our Int, so try widest first.
            if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                v.map_or(Value::Null, Value::Int)
            } else if let Ok(v) = row.try_get::<Option<i32>, _>(i) {
                v.map_or(Value::Null, |x| Value::Int(i64::from(x)))
            } else {
                row.try_get::<Option<i16>, _>(i)
                    .map_err(|e| column_err(&name(), &e))?
                    .map_or(Value::Null, |x| Value::Int(i64::from(x)))
            }
        }
        DataType::Float => {
            if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
                v.map_or(Value::Null, Value::Float)
            } else {
                row.try_get::<Option<f32>, _>(i)
                    .map_err(|e| column_err(&name(), &e))?
                    .map_or(Value::Null, |x| Value::Float(f64::from(x)))
            }
        }
        DataType::Decimal => row
            .try_get::<Option<rust_decimal::Decimal>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, Value::Decimal),
        DataType::Text => row
            .try_get::<Option<String>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, Value::Text),
        DataType::Timestamp => row
            .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, |t| Value::Timestamp(from_chrono(t))),
        DataType::Uuid => row
            .try_get::<Option<uuid::Uuid>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, Value::Uuid),
        DataType::Json => row
            .try_get::<Option<serde_json::Value>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, Value::Json),
    })
}

fn to_chrono(t: jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(t.as_second(), t.subsec_nanosecond().unsigned_abs())
        .unwrap_or_default()
}

fn from_chrono(t: chrono::DateTime<chrono::Utc>) -> jiff::Timestamp {
    let nanos = i32::try_from(t.timestamp_subsec_nanos()).unwrap_or(0);
    jiff::Timestamp::new(t.timestamp(), nanos).unwrap_or_default()
}

/// Map a PostgreSQL type name onto Sluice's value model.
///
/// Returning `None` means "not modelled": the column is dropped from the
/// schema, and any action that names it fails validation with a clear message
/// rather than returning something mis-decoded at runtime.
fn pg_type(udt: &str) -> Option<DataType> {
    Some(match udt {
        "bool" => DataType::Bool,
        "int2" | "int4" | "int8" => DataType::Int,
        "float4" | "float8" => DataType::Float,
        "numeric" | "money" => DataType::Decimal,
        "text" | "varchar" | "bpchar" | "name" | "citext" => DataType::Text,
        "timestamp" | "timestamptz" | "date" => DataType::Timestamp,
        "uuid" => DataType::Uuid,
        "json" | "jsonb" => DataType::Json,
        _ => return None,
    })
}

fn decode_err(e: &sqlx::Error) -> Error {
    Error::Backend(format!("unexpected catalogue shape: {e}"))
}

fn column_err(name: &str, e: &sqlx::Error) -> Error {
    Error::Backend(format!("column `{name}` could not be decoded: {e}"))
}

/// Keep server error text useful without echoing bound values back to a model.
fn sanitise(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => match db.code() {
            Some(code) => format!("database rejected the statement ({code}): {}", db.message()),
            None => format!("database rejected the statement: {}", db.message()),
        },
        sqlx::Error::PoolTimedOut => {
            "no database connection was free within the pool timeout".into()
        }
        other => format!("database error: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmodelled_types_are_dropped_not_guessed() {
        assert_eq!(pg_type("int4"), Some(DataType::Int));
        assert_eq!(pg_type("jsonb"), Some(DataType::Json));
        assert_eq!(pg_type("_text"), None, "arrays are not modelled");
        assert_eq!(pg_type("tsvector"), None);
    }

    #[test]
    fn timestamps_survive_the_round_trip() {
        let t: jiff::Timestamp = "2026-09-27T10:11:12.123456789Z".parse().unwrap();
        assert_eq!(from_chrono(to_chrono(t)), t);
    }
}
