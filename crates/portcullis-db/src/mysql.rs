//! The MySQL backend (also MariaDB).
//!
//! Two things differ from PostgreSQL and both are handled here rather than
//! leaking into the engine. Placeholders are positional `?`, which the dialect
//! covers. And there is no `RETURNING`, so an action that asks for values back
//! gets a follow-up `SELECT` on the same connection, keyed either by the
//! primary key values the write itself supplied or by `LAST_INSERT_ID()`.
//!
//! MySQL has no UUID type. A `uuid` column is expected to be `CHAR(36)`, and
//! UUID values are bound and decoded as text.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use portcullis_core::{Column, DataType, Error, Result, Schema, Table, Value};
use portcullis_sql::{Binder, Dialect, MySql as MySqlDialect, ReadQuery, WriteQuery};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow};
use sqlx::{AssertSqlSafe, ConnectOptions, Executor, Pool, Row as _};
use tokio::sync::RwLock;

use crate::Backend;
use crate::plan::{ExecCtx, ReadPlan, Rows, WriteOutcome, WritePlan};

/// How to reach a MySQL server.
#[derive(Debug, Clone)]
pub struct MySqlConfig {
    /// Connection string.
    pub dsn: String,
    /// Maximum pooled connections.
    pub max_connections: u32,
    /// Minimum idle connections kept warm.
    pub min_connections: u32,
    /// How long to wait for a connection from the pool.
    pub acquire_timeout: Duration,
    /// Databases to expose. Empty means the one in the connection string.
    pub schemas: Vec<String>,
    /// Server-side backstop on every connection.
    pub statement_timeout: Duration,
}

impl Default for MySqlConfig {
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

/// A pooled MySQL backend.
#[derive(Debug)]
pub struct MySqlBackend {
    pool: Pool<sqlx::MySql>,
    dialect: MySqlDialect,
    schema: RwLock<Schema>,
    label: String,
    schemas: Vec<String>,
}

impl MySqlBackend {
    /// Connect, verify the server answers and read the schema once.
    pub async fn connect(cfg: &MySqlConfig) -> Result<Self> {
        let opts: MySqlConnectOptions = cfg
            .dsn
            .parse()
            .map_err(|e| Error::Config(format!("connection string is not valid: {e}")))?;
        let label = format!(
            "mysql://{}@{}:{}/{}",
            opts.get_username(),
            opts.get_host(),
            opts.get_port(),
            opts.get_database().unwrap_or("")
        );
        let millis = u64::try_from(cfg.statement_timeout.as_millis()).unwrap_or(60_000);

        let pool = MySqlPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(cfg.acquire_timeout)
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    // MariaDB spells this differently and simply ignores an
                    // unknown variable set this way, so a failure here is not
                    // worth refusing the connection over.
                    let _ = conn
                        .execute(AssertSqlSafe(format!(
                            "SET SESSION max_execution_time = {millis}"
                        )))
                        .await;
                    Ok(())
                })
            })
            .connect_with(opts.disable_statement_logging())
            .await
            .map_err(|e| Error::Backend(format!("cannot connect to {label}: {e}")))?;

        let backend = Self {
            pool,
            dialect: MySqlDialect,
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

    #[allow(
        clippy::too_many_lines,
        reason = "two catalogue queries and their row mapping, which read better together"
    )]
    async fn read_schema(&self) -> Result<Schema> {
        let scope = if self.schemas.is_empty() {
            "c.table_schema = DATABASE()".to_owned()
        } else {
            let list = self
                .schemas
                .iter()
                .map(|s| format!("'{}'", s.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("c.table_schema IN ({list})")
        };

        let sql = format!(
            "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, \
                    c.is_nullable, c.extra, c.column_default \
             FROM information_schema.columns c \
             JOIN information_schema.tables t \
               ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
             WHERE {scope} AND t.table_type IN ('BASE TABLE', 'VIEW') \
             ORDER BY c.table_schema, c.table_name, c.ordinal_position"
        );

        let rows = sqlx::query(AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::Backend(format!("reading the schema failed: {e}")))?;

        let mut tables: BTreeMap<String, Table> = BTreeMap::new();
        for row in rows {
            let schema: String = row
                .try_get("TABLE_SCHEMA")
                .or_else(|_| row.try_get("table_schema"))
                .map_err(|e| decode_err(&e))?;
            let name: String = row
                .try_get("TABLE_NAME")
                .or_else(|_| row.try_get("table_name"))
                .map_err(|e| decode_err(&e))?;
            let column: String = row
                .try_get("COLUMN_NAME")
                .or_else(|_| row.try_get("column_name"))
                .map_err(|e| decode_err(&e))?;
            let data_type: String = row
                .try_get("DATA_TYPE")
                .or_else(|_| row.try_get("data_type"))
                .map_err(|e| decode_err(&e))?;
            let nullable: String = row
                .try_get("IS_NULLABLE")
                .or_else(|_| row.try_get("is_nullable"))
                .map_err(|e| decode_err(&e))?;
            let extra: String = row
                .try_get("EXTRA")
                .or_else(|_| row.try_get("extra"))
                .unwrap_or_default();
            let default: Option<String> = row
                .try_get("COLUMN_DEFAULT")
                .or_else(|_| row.try_get("column_default"))
                .unwrap_or(None);

            let Some(ty) = mysql_type(&data_type.to_ascii_lowercase()) else {
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
                    generated: extra.to_ascii_lowercase().contains("auto_increment")
                        || extra.to_ascii_lowercase().contains("generated")
                        || default.is_some(),
                });
        }

        let pk_sql = "SELECT k.table_schema, k.table_name, k.column_name \
             FROM information_schema.key_column_usage k \
             JOIN information_schema.table_constraints t \
               ON t.constraint_name = k.constraint_name \
              AND t.table_schema = k.table_schema \
              AND t.table_name = k.table_name \
             WHERE t.constraint_type = 'PRIMARY KEY' \
             ORDER BY k.ordinal_position";

        for row in sqlx::query(pk_sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::Backend(format!("reading primary keys failed: {e}")))?
        {
            let schema: String = row
                .try_get("TABLE_SCHEMA")
                .or_else(|_| row.try_get("table_schema"))
                .map_err(|e| decode_err(&e))?;
            let name: String = row
                .try_get("TABLE_NAME")
                .or_else(|_| row.try_get("table_name"))
                .map_err(|e| decode_err(&e))?;
            let column: String = row
                .try_get("COLUMN_NAME")
                .or_else(|_| row.try_get("column_name"))
                .map_err(|e| decode_err(&e))?;
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
impl Backend for MySqlBackend {
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

        let mut query = sqlx::query(AssertSqlSafe(stmt.sql.as_str()));
        for v in &stmt.binds {
            query = bind(query, v);
        }
        let rows = tokio::time::timeout(ctx.timeout, query.fetch_all(&self.pool))
            .await
            .map_err(|_| Error::LimitExceeded(format!("statement exceeded {:?}", ctx.timeout)))?
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        Ok(Rows {
            columns: plan.columns.to_vec(),
            rows: rows
                .iter()
                .map(|r| decode_row(r, &types))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    async fn write(&self, plan: &WritePlan<'_>, ctx: &ExecCtx<'_>) -> Result<WriteOutcome> {
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

        // One connection for the write and the read-back, so LAST_INSERT_ID()
        // means what it should.
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        let mut query = sqlx::query(AssertSqlSafe(written.statement.sql.as_str()));
        for v in &written.statement.binds {
            query = bind(query, v);
        }
        let done = tokio::time::timeout(ctx.timeout, query.execute(&mut *conn))
            .await
            .map_err(|_| Error::LimitExceeded(format!("statement exceeded {:?}", ctx.timeout)))?
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        if plan.returning.is_empty() {
            return Ok(WriteOutcome {
                rows_affected: done.rows_affected(),
                returned: Rows::default(),
            });
        }

        // MySQL cannot return the row, so find it again.
        let key_columns = self.primary_key(plan.table).await;
        let table = self.dialect.quote(plan.table)?;
        let projection = plan
            .returning
            .iter()
            .map(|c| self.dialect.quote(c))
            .collect::<Result<Vec<_>>>()?
            .join(", ");

        let known: Vec<(&String, &Value)> = key_columns
            .iter()
            .filter_map(|k| written.values.get(k).map(|v| (k, v)))
            .collect();

        let (sql, binds): (String, Vec<Value>) = if known.len() == key_columns.len()
            && !key_columns.is_empty()
        {
            let wheres = known
                .iter()
                .map(|(k, _)| Ok(format!("{} = ?", self.dialect.quote(k)?)))
                .collect::<Result<Vec<_>>>()?
                .join(" AND ");
            (
                format!("SELECT {projection} FROM {table} WHERE {wheres}"),
                known.iter().map(|(_, v)| (*v).clone()).collect(),
            )
        } else if key_columns.len() == 1 && done.last_insert_id() != 0 {
            let key = self.dialect.quote(&key_columns[0])?;
            (
                format!("SELECT {projection} FROM {table} WHERE {key} = ?"),
                vec![Value::Int(
                    i64::try_from(done.last_insert_id()).unwrap_or(i64::MAX),
                )],
            )
        } else {
            return Err(Error::Backend(format!(
                "`{}` asks for columns back, but MySQL cannot return them: `{}` has no primary key this write supplies. Drop `returning`, or give the table a key the action sets.",
                plan.table, plan.table
            )));
        };

        let types = self.column_types(plan.table, plan.returning).await?;
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for v in &binds {
            query = bind(query, v);
        }
        let rows = query
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        Ok(WriteOutcome {
            rows_affected: done.rows_affected(),
            returned: Rows {
                columns: plan.returning.to_vec(),
                rows: rows
                    .iter()
                    .map(|r| decode_row(r, &types))
                    .collect::<Result<Vec<_>>>()?,
            },
        })
    }

    async fn health(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| Error::Backend(sanitise(&e)))
    }

    async fn supports_shared_state(&self) -> Result<bool> {
        let row: (i64,) = sqlx::query_as(
            "SELECT count(*) FROM information_schema.tables              WHERE table_schema = DATABASE()                AND table_name IN ('portcullis_rate', 'portcullis_replay')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::Backend(sanitise(&e)))?;
        Ok(row.0 >= 2)
    }

    async fn rate_check(&self, caller: &str, action: &str, per_minute: u32) -> Result<bool> {
        let bucket = jiff::Timestamp::now().as_second() / 60;
        // MySQL has no RETURNING, so the upsert and the read share one
        // connection: another replica must not slip between them.
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| Error::Backend(sanitise(&e)))?;

        sqlx::query(
            "INSERT INTO portcullis_rate (caller, action, bucket, hits) VALUES (?, ?, ?, 1)              ON DUPLICATE KEY UPDATE hits = hits + 1",
        )
        .bind(caller)
        .bind(action)
        .bind(bucket)
        .execute(&mut *conn)
        .await
        .map_err(|e| Error::Backend(sanitise(&e)))?;

        let row: (i32,) = sqlx::query_as(
            "SELECT hits FROM portcullis_rate WHERE caller = ? AND action = ? AND bucket = ?",
        )
        .bind(caller)
        .bind(action)
        .bind(bucket)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| Error::Backend(sanitise(&e)))?;

        let _ = sqlx::query("DELETE FROM portcullis_rate WHERE bucket < ?")
            .bind(bucket - 5)
            .execute(&mut *conn)
            .await;

        Ok(u32::try_from(row.0).unwrap_or(u32::MAX) <= per_minute)
    }

    async fn replay_get(&self, key: &str, ttl: Duration) -> Result<Option<serde_json::Value>> {
        let cutoff =
            jiff::Timestamp::now().as_second() - i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let row: Option<(String,)> =
            sqlx::query_as("SELECT response FROM portcullis_replay WHERE `key` = ? AND at > ?")
                .bind(key)
                .bind(cutoff)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::Backend(sanitise(&e)))?;
        Ok(row.and_then(|(text,)| serde_json::from_str(&text).ok()))
    }

    async fn replay_put(
        &self,
        key: &str,
        action: &str,
        response: &serde_json::Value,
        ttl: Duration,
    ) -> Result<()> {
        let now = jiff::Timestamp::now().as_second();
        // IGNORE rather than overwrite: the first answer is the one the caller
        // already has, and a retry must keep getting it.
        sqlx::query(
            "INSERT IGNORE INTO portcullis_replay (`key`, action, response, at)              VALUES (?, ?, ?, ?)",
        )
        .bind(key)
        .bind(action)
        .bind(response.to_string())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Backend(sanitise(&e)))?;

        let cutoff = now - i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let _ = sqlx::query("DELETE FROM portcullis_replay WHERE at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await;
        Ok(())
    }
}

type MyQuery<'q> = sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>;

fn bind<'q>(q: MyQuery<'q>, v: &'q Value) -> MyQuery<'q> {
    match v {
        Value::Null => q,
        Value::Bool(b) => q.bind(b),
        Value::Int(i) => q.bind(i),
        Value::Float(f) => q.bind(f),
        Value::Decimal(d) => q.bind(d),
        Value::Text(s) => q.bind(s),
        Value::Timestamp(t) => q.bind(to_chrono(*t)),
        // MySQL has no UUID type; CHAR(36) is the convention.
        Value::Uuid(u) => q.bind(u.to_string()),
        Value::Json(j) => q.bind(j),
    }
}

fn decode_row(row: &MySqlRow, types: &[DataType]) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(types.len());
    for (i, ty) in types.iter().enumerate() {
        out.push(decode(row, i, *ty)?);
    }
    Ok(out)
}

fn decode(row: &MySqlRow, i: usize, ty: DataType) -> Result<Value> {
    let name = || {
        row.columns()
            .get(i)
            .map_or("?", sqlx::Column::name)
            .to_owned()
    };
    Ok(match ty {
        DataType::Bool => {
            // MySQL BOOLEAN is TINYINT(1).
            if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
                v.map_or(Value::Null, Value::Bool)
            } else {
                row.try_get::<Option<i8>, _>(i)
                    .map_err(|e| column_err(&name(), &e))?
                    .map_or(Value::Null, |n| Value::Bool(n != 0))
            }
        }
        DataType::Int => {
            if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                v.map_or(Value::Null, Value::Int)
            } else if let Ok(v) = row.try_get::<Option<i32>, _>(i) {
                v.map_or(Value::Null, |x| Value::Int(i64::from(x)))
            } else {
                row.try_get::<Option<u64>, _>(i)
                    .map_err(|e| column_err(&name(), &e))?
                    .map_or(Value::Null, |x| {
                        Value::Int(i64::try_from(x).unwrap_or(i64::MAX))
                    })
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
        DataType::Timestamp => {
            // TIMESTAMP comes back with a zone; DATETIME does not, and is
            // read as UTC because that is the only defensible guess.
            if let Ok(v) = row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(i) {
                v.map_or(Value::Null, |t| Value::Timestamp(from_chrono(t)))
            } else {
                row.try_get::<Option<chrono::NaiveDateTime>, _>(i)
                    .map_err(|e| column_err(&name(), &e))?
                    .map_or(Value::Null, |t| Value::Timestamp(from_chrono(t.and_utc())))
            }
        }
        DataType::Uuid => row
            .try_get::<Option<String>, _>(i)
            .map_err(|e| column_err(&name(), &e))?
            .map_or(Value::Null, |s| {
                uuid::Uuid::parse_str(&s).map_or(Value::Text(s), Value::Uuid)
            }),
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

/// Map a MySQL type name onto Portcullis's value model.
///
/// `None` means the column is dropped from the schema rather than guessed at.
/// `char(36)` holding a UUID is modelled as text; declare the parameter as
/// `text` and it round-trips.
fn mysql_type(data_type: &str) -> Option<DataType> {
    Some(match data_type {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            DataType::Int
        }
        "bool" | "boolean" => DataType::Bool,
        "float" | "double" | "real" => DataType::Float,
        "decimal" | "numeric" => DataType::Decimal,
        "char" | "varchar" | "text" | "tinytext" | "mediumtext" | "longtext" | "enum" | "set" => {
            DataType::Text
        }
        "date" | "datetime" | "timestamp" => DataType::Timestamp,
        "json" => DataType::Json,
        "uuid" => DataType::Uuid,
        _ => return None,
    })
}

fn decode_err(e: &sqlx::Error) -> Error {
    Error::Backend(format!("unexpected catalogue shape: {e}"))
}

fn column_err(name: &str, e: &sqlx::Error) -> Error {
    Error::Backend(format!("column `{name}` could not be decoded: {e}"))
}

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
        assert_eq!(mysql_type("bigint"), Some(DataType::Int));
        assert_eq!(mysql_type("varchar"), Some(DataType::Text));
        assert_eq!(mysql_type("datetime"), Some(DataType::Timestamp));
        assert_eq!(mysql_type("blob"), None);
        assert_eq!(mysql_type("geometry"), None);
    }

    #[test]
    fn timestamps_survive_the_round_trip() {
        let t: jiff::Timestamp = "2026-09-27T10:11:12Z".parse().unwrap();
        assert_eq!(from_chrono(to_chrono(t)), t);
    }
}
