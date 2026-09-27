//! The Microsoft SQL Server backend.
//!
//! This is the one backend that does not go through `sqlx`: MSSQL support was
//! dropped after sqlx 0.6, so the driver here is `tiberius` with a `deadpool`
//! pool. Nothing above the backend notices, because the boundary is a plan
//! rather than SQL text.
//!
//! Three things differ from the other two engines, all handled here:
//!
//! - Placeholders are `@P1`, `@P2`, and identifiers use `[brackets]`.
//! - The row limit is `TOP (n)` at the front. T-SQL's `OFFSET/FETCH` needs an
//!   `ORDER BY` that an action may not have.
//! - There is no `RETURNING`. `OUTPUT INSERTED` exists but sits mid-statement,
//!   so, as on MySQL, the row is read back on the same connection.
//!
//! Upserts are refused rather than implemented. T-SQL spells it `MERGE`, which
//! is a different statement shape, and the naive
//! `IF EXISTS … UPDATE ELSE INSERT` races. An upsert that occasionally writes
//! twice is worse than one that is not offered.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use portcullis_core::{Column, DataType, Error, Result, Schema, Table, Value, WriteMode};
use portcullis_sql::{Binder, Dialect, ReadQuery, SqlServer, WriteQuery};
use tiberius::{Row, ToSql};
use tokio::sync::RwLock;

use crate::Backend;
use crate::plan::{ExecCtx, ReadPlan, Rows, WriteOutcome, WritePlan};

/// How to reach a SQL Server instance.
#[derive(Debug, Clone)]
pub struct MsSqlConfig {
    /// ADO-style connection string.
    pub dsn: String,
    /// Maximum pooled connections.
    pub max_connections: usize,
    /// How long to wait for a connection from the pool.
    pub acquire_timeout: Duration,
    /// Schemas to expose. Empty means every non-system schema.
    pub schemas: Vec<String>,
}

impl Default for MsSqlConfig {
    fn default() -> Self {
        Self {
            dsn: String::new(),
            max_connections: 10,
            acquire_timeout: Duration::from_secs(10),
            schemas: Vec::new(),
        }
    }
}

/// A connection checked out of the pool.
type Conn = deadpool_tiberius::deadpool::managed::Object<deadpool_tiberius::Manager>;

/// A pooled SQL Server backend.
pub struct MsSqlBackend {
    pool: deadpool_tiberius::Pool,
    dialect: SqlServer,
    schema: RwLock<Schema>,
    label: String,
    schemas: Vec<String>,
}

// The pool's manager is not Debug, and the trait needs one. Printing the
// label is all a log wants anyway; the password is already out of it.
impl std::fmt::Debug for MsSqlBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsSqlBackend")
            .field("server", &self.label)
            .finish_non_exhaustive()
    }
}

impl MsSqlBackend {
    /// Connect, verify the server answers and read the schema once.
    pub async fn connect(cfg: &MsSqlConfig) -> Result<Self> {
        let pool = deadpool_tiberius::Manager::from_ado_string(&cfg.dsn)
            .map_err(|e| Error::Config(format!("connection string is not valid: {e}")))?
            .max_size(cfg.max_connections)
            .wait_timeout(cfg.acquire_timeout)
            .create_pool()
            .map_err(|e| Error::Backend(format!("cannot build the pool: {e}")))?;

        let backend = Self {
            pool,
            dialect: SqlServer,
            schema: RwLock::new(Schema::default()),
            label: redact(&cfg.dsn),
            schemas: cfg.schemas.clone(),
        };
        // Fail here rather than at the first request.
        backend.health().await?;
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
        let scope = if self.schemas.is_empty() {
            "c.TABLE_SCHEMA NOT IN ('sys', 'INFORMATION_SCHEMA')".to_owned()
        } else {
            let list = self
                .schemas
                .iter()
                .map(|s| format!("'{}'", s.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("c.TABLE_SCHEMA IN ({list})")
        };

        let sql = format!(
            "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, c.COLUMN_NAME, c.DATA_TYPE, \
                    c.IS_NULLABLE, c.COLUMN_DEFAULT, \
                    COLUMNPROPERTY(OBJECT_ID(QUOTENAME(c.TABLE_SCHEMA) + '.' + QUOTENAME(c.TABLE_NAME)), \
                                   c.COLUMN_NAME, 'IsIdentity') AS IS_IDENTITY \
             FROM INFORMATION_SCHEMA.COLUMNS c \
             JOIN INFORMATION_SCHEMA.TABLES t \
               ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
             WHERE {scope} AND t.TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
             ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION"
        );

        let mut conn = self.conn().await?;
        let rows = conn
            .query(&sql, &[])
            .await
            .map_err(|e| Error::Backend(format!("reading the schema failed: {e}")))?
            .into_first_result()
            .await
            .map_err(|e| Error::Backend(format!("reading the schema failed: {e}")))?;

        let mut tables: BTreeMap<String, Table> = BTreeMap::new();
        for row in rows {
            let schema = text(&row, 0).unwrap_or_default();
            let name = text(&row, 1).unwrap_or_default();
            let column = text(&row, 2).unwrap_or_default();
            let data_type = text(&row, 3).unwrap_or_default().to_ascii_lowercase();
            let nullable = text(&row, 4).unwrap_or_default();
            let default: Option<String> = text(&row, 5);
            let identity: Option<i32> = row.get(6);

            let Some(ty) = mssql_type(&data_type) else {
                continue;
            };

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
                    nullable: nullable.eq_ignore_ascii_case("YES"),
                    generated: identity == Some(1) || default.is_some(),
                });
        }

        let pk_sql = "SELECT k.TABLE_SCHEMA, k.TABLE_NAME, k.COLUMN_NAME \
             FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE k \
             JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS t \
               ON t.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
              AND t.TABLE_SCHEMA = k.TABLE_SCHEMA \
             WHERE t.CONSTRAINT_TYPE = 'PRIMARY KEY' \
             ORDER BY k.ORDINAL_POSITION";

        let pk_rows = conn
            .query(pk_sql, &[])
            .await
            .map_err(|e| Error::Backend(format!("reading primary keys failed: {e}")))?
            .into_first_result()
            .await
            .map_err(|e| Error::Backend(format!("reading primary keys failed: {e}")))?;

        for row in pk_rows {
            let schema = text(&row, 0).unwrap_or_default();
            let name = text(&row, 1).unwrap_or_default();
            let column = text(&row, 2).unwrap_or_default();
            if let Some(t) = tables.get_mut(&format!("{schema}.{name}")) {
                t.primary_key.push(column);
            }
        }

        Ok(Schema { tables })
    }

    // Boxed on purpose. deadpool's checkout future is around 20 KB, and it
    // is awaited on every request path, so leaving it inline grows every
    // future above it right up to `Engine::build`.
    async fn conn(&self) -> Result<Conn> {
        Box::pin(self.pool.get())
            .await
            .map_err(|e| Error::Backend(format!("no database connection was free: {e}")))
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

    async fn primary_key(&self, table: &str) -> Vec<String> {
        self.schema
            .read()
            .await
            .table(table)
            .map(|t| t.primary_key.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Backend for MsSqlBackend {
    fn describe(&self) -> String {
        self.label.clone()
    }

    async fn schema(&self) -> Result<Schema> {
        Ok(self.schema.read().await.clone())
    }

    async fn read(&self, plan: &ReadPlan<'_>, ctx: &ExecCtx<'_>) -> Result<Rows> {
        let mut binder = Binder::new(ctx.args, ctx.caller);
        let stmt = portcullis_sql::select(
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

        let owned = Params::new(&stmt.binds);
        let mut conn = self.conn().await?;
        let rows = tokio::time::timeout(ctx.timeout, conn.query(&stmt.sql, &owned.as_refs()))
            .await
            .map_err(|_| Error::LimitExceeded(format!("statement exceeded {:?}", ctx.timeout)))?
            .map_err(|e| Error::Backend(sanitise(&e)))?
            .into_first_result()
            .await
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        Ok(Rows {
            columns: plan.columns.to_vec(),
            rows: rows.iter().map(|r| decode_row(r, &types)).collect(),
        })
    }

    async fn write(&self, plan: &WritePlan<'_>, ctx: &ExecCtx<'_>) -> Result<WriteOutcome> {
        if plan.mode == WriteMode::Upsert {
            return Err(Error::Backend(
                "SQL Server upserts are not implemented; use mode = \"insert\" or \"update\""
                    .into(),
            ));
        }

        let mut binder = Binder::new(ctx.args, ctx.caller);
        let written = portcullis_sql::write(
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

        // One connection for the write and the read-back.
        let mut conn = self.conn().await?;
        let owned = Params::new(&written.statement.binds);
        let done = tokio::time::timeout(
            ctx.timeout,
            conn.execute(&written.statement.sql, &owned.as_refs()),
        )
        .await
        .map_err(|_| Error::LimitExceeded(format!("statement exceeded {:?}", ctx.timeout)))?
        .map_err(|e| Error::Backend(sanitise(&e)))?;
        let affected: u64 = done.rows_affected().iter().sum();

        if plan.returning.is_empty() {
            return Ok(WriteOutcome {
                rows_affected: affected,
                returned: Rows::default(),
            });
        }

        // No RETURNING, so find the row again by the key values this write
        // supplied. Identity columns are not covered: an action that wants
        // them back should supply its own key.
        let key_columns = self.primary_key(plan.table).await;
        let known: Vec<(&String, &Value)> = key_columns
            .iter()
            .filter_map(|k| written.values.get(k).map(|v| (k, v)))
            .collect();
        if key_columns.is_empty() || known.len() != key_columns.len() {
            return Err(Error::Backend(format!(
                "`{}` asks for columns back, but SQL Server cannot return them: `{}` has no primary key this write supplies. Drop `returning`, or set the key in `write.columns`.",
                plan.table, plan.table
            )));
        }

        let table = self.dialect.quote(plan.table)?;
        let projection = plan
            .returning
            .iter()
            .map(|c| self.dialect.quote(c))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let wheres = known
            .iter()
            .enumerate()
            .map(|(i, (k, _))| Ok(format!("{} = @P{}", self.dialect.quote(k)?, i + 1)))
            .collect::<Result<Vec<_>>>()?
            .join(" AND ");
        let sql = format!("SELECT {projection} FROM {table} WHERE {wheres}");

        let values: Vec<Value> = known.iter().map(|(_, v)| (*v).clone()).collect();
        let owned = Params::new(&values);
        let types = self.column_types(plan.table, plan.returning).await?;
        let rows = conn
            .query(&sql, &owned.as_refs())
            .await
            .map_err(|e| Error::Backend(sanitise(&e)))?
            .into_first_result()
            .await
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        Ok(WriteOutcome {
            rows_affected: affected,
            returned: Rows {
                columns: plan.returning.to_vec(),
                rows: rows.iter().map(|r| decode_row(r, &types)).collect(),
            },
        })
    }

    async fn health(&self) -> Result<()> {
        let mut conn = self.conn().await?;
        conn.simple_query("SELECT 1")
            .await
            .map(|_| ())
            .map_err(|e| Error::Backend(sanitise(&e)))
    }
}

/// Owned parameter values, so the borrowed `&dyn ToSql` slice tiberius wants
/// has something to point at.
struct Params {
    owned: Vec<Owned>,
}

enum Owned {
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(rust_decimal::Decimal),
    Text(String),
    Time(chrono::NaiveDateTime),
    Uuid(uuid::Uuid),
}

impl Params {
    fn new(values: &[Value]) -> Self {
        let owned = values
            .iter()
            .map(|v| match v {
                Value::Bool(b) => Owned::Bool(*b),
                Value::Int(i) => Owned::Int(*i),
                Value::Float(f) => Owned::Float(*f),
                Value::Decimal(d) => Owned::Decimal(*d),
                Value::Timestamp(t) => Owned::Time(to_chrono(*t).naive_utc()),
                Value::Uuid(u) => Owned::Uuid(*u),
                // Null never reaches here: the builder writes the keyword.
                // JSON has no native type in SQL Server, so it travels as the
                // text it is stored as.
                Value::Text(s) => Owned::Text(s.clone()),
                Value::Json(j) => Owned::Text(j.to_string()),
                Value::Null => Owned::Text(String::new()),
            })
            .collect();
        Self { owned }
    }

    fn as_refs(&self) -> Vec<&dyn ToSql> {
        self.owned
            .iter()
            .map(|o| match o {
                Owned::Bool(b) => b as &dyn ToSql,
                Owned::Int(i) => i as &dyn ToSql,
                Owned::Float(f) => f as &dyn ToSql,
                Owned::Decimal(d) => d as &dyn ToSql,
                Owned::Text(s) => s as &dyn ToSql,
                Owned::Time(t) => t as &dyn ToSql,
                Owned::Uuid(u) => u as &dyn ToSql,
            })
            .collect()
    }
}

fn text(row: &Row, i: usize) -> Option<String> {
    row.get::<&str, _>(i).map(ToOwned::to_owned)
}

fn decode_row(row: &Row, types: &[DataType]) -> Vec<Value> {
    types
        .iter()
        .enumerate()
        .map(|(i, ty)| decode(row, i, *ty))
        .collect()
}

fn decode(row: &Row, i: usize, ty: DataType) -> Value {
    match ty {
        DataType::Bool => row.get::<bool, _>(i).map_or(Value::Null, Value::Bool),
        DataType::Int => row
            .get::<i64, _>(i)
            .or_else(|| row.get::<i32, _>(i).map(i64::from))
            .or_else(|| row.get::<i16, _>(i).map(i64::from))
            .or_else(|| row.get::<u8, _>(i).map(i64::from))
            .map_or(Value::Null, Value::Int),
        DataType::Float => row
            .get::<f64, _>(i)
            .or_else(|| row.get::<f32, _>(i).map(f64::from))
            .map_or(Value::Null, Value::Float),
        DataType::Decimal => row
            .get::<rust_decimal::Decimal, _>(i)
            .map_or(Value::Null, Value::Decimal),
        DataType::Text => row
            .get::<&str, _>(i)
            .map_or(Value::Null, |s| Value::Text(s.to_owned())),
        DataType::Timestamp => row
            .get::<chrono::NaiveDateTime, _>(i)
            .map_or(Value::Null, |t| Value::Timestamp(from_chrono(t.and_utc()))),
        DataType::Uuid => row.get::<uuid::Uuid, _>(i).map_or(Value::Null, Value::Uuid),
        DataType::Json => row.get::<&str, _>(i).map_or(Value::Null, |s| {
            serde_json::from_str(s).map_or_else(|_| Value::Text(s.to_owned()), Value::Json)
        }),
    }
}

fn to_chrono(t: jiff::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(t.as_second(), t.subsec_nanosecond().unsigned_abs())
        .unwrap_or_default()
}

fn from_chrono(t: chrono::DateTime<chrono::Utc>) -> jiff::Timestamp {
    let nanos = i32::try_from(t.timestamp_subsec_nanos()).unwrap_or(0);
    jiff::Timestamp::new(t.timestamp(), nanos).unwrap_or_default()
}

/// Map a SQL Server type name onto Portcullis's value model.
///
/// `None` means the column is dropped from the schema rather than guessed at.
/// SQL Server has no JSON type; JSON lives in `nvarchar`, so declare those
/// columns as text.
fn mssql_type(data_type: &str) -> Option<DataType> {
    Some(match data_type {
        "bit" => DataType::Bool,
        "tinyint" | "smallint" | "int" | "bigint" => DataType::Int,
        "float" | "real" => DataType::Float,
        "decimal" | "numeric" | "money" | "smallmoney" => DataType::Decimal,
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" => DataType::Text,
        "date" | "datetime" | "datetime2" | "smalldatetime" => DataType::Timestamp,
        "uniqueidentifier" => DataType::Uuid,
        _ => return None,
    })
}

/// A connection string with the password taken out, for logs.
fn redact(dsn: &str) -> String {
    let kept: Vec<&str> = dsn
        .split(';')
        .filter(|part| {
            let key = part
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            !matches!(key.as_str(), "password" | "pwd")
        })
        .filter(|p| !p.trim().is_empty())
        .collect();
    format!("sqlserver:{}", kept.join(";"))
}

fn sanitise(e: &tiberius::error::Error) -> String {
    match e {
        tiberius::error::Error::Server(token) => format!(
            "database rejected the statement ({}): {}",
            token.code(),
            token.message()
        ),
        other => format!("database error: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmodelled_types_are_dropped_not_guessed() {
        assert_eq!(mssql_type("bigint"), Some(DataType::Int));
        assert_eq!(mssql_type("nvarchar"), Some(DataType::Text));
        assert_eq!(mssql_type("datetime2"), Some(DataType::Timestamp));
        assert_eq!(mssql_type("uniqueidentifier"), Some(DataType::Uuid));
        assert_eq!(mssql_type("varbinary"), None);
        assert_eq!(mssql_type("geography"), None);
        assert_eq!(mssql_type("xml"), None);
    }

    #[test]
    fn the_label_keeps_the_host_and_drops_the_password() {
        let label = redact("Server=tcp:db,1433;User Id=sa;Password=hunter2;Database=orders;");
        assert!(label.contains("Server=tcp:db,1433"));
        assert!(label.contains("Database=orders"));
        assert!(!label.contains("hunter2"), "{label}");
    }

    #[test]
    fn timestamps_survive_the_round_trip() {
        let t: jiff::Timestamp = "2026-09-28T10:11:12Z".parse().unwrap();
        assert_eq!(from_chrono(to_chrono(t)), t);
    }
}
