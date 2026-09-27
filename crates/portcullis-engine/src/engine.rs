//! The request path.
//!
//! Every call goes through the same sequence, in this order: resolve the
//! action, check the role, check the rate limit, type-check the arguments,
//! apply the approval gate, check for a replay, execute, mask, audit. Nothing
//! reaches the database until each earlier step has passed, and the audit
//! record is written whether the call succeeded, was refused or was parked.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use portcullis_core::audit::{AuditRecord, Decision};
use portcullis_core::{
    ActionKind, AuditLog, Caller, Error, Mask, Result, Schema, Value, did_you_mean,
};
use portcullis_db::plan::{ExecCtx, ReadPlan, Rows, WritePlan};
use portcullis_db::{Backend, MemoryBackend};

use crate::approvals::{Approval, ApprovalStore, Status};
use crate::config::{BackendConfig, Config};
use crate::limits::{IdempotencyStore, RateLimiter};
use crate::registry::{Action, Registry, Warning, coerce_argument};

/// The one action the engine answers itself rather than looking up.
///
/// An approved write returns its result to whoever released it, not to the
/// agent that asked, so without this a caller has no way to find out what
/// happened to a request it raised. Reserved: a configured action may not use
/// this name.
pub const STATUS_ACTION: &str = "approval_status";

/// What a successful call produced.
#[derive(Debug, Clone)]
pub struct CallResult {
    /// Action that ran.
    pub action: String,
    /// Read or write.
    pub kind: ActionKind,
    /// Correlates with the audit log.
    pub request_id: String,
    /// Rows returned, already masked.
    pub rows: Rows,
    /// Rows changed by a write.
    pub rows_affected: u64,
    /// True when the row limit cut the result short.
    pub truncated: bool,
    /// True when this was a replay of an earlier identical write.
    pub replayed: bool,
    /// How long the call took.
    pub duration: Duration,
}

impl CallResult {
    /// The payload handed back to an MCP client.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "action".into(),
            serde_json::Value::String(self.action.clone()),
        );
        match self.kind {
            ActionKind::Read => {
                obj.insert("rows".into(), self.rows.to_json());
                obj.insert("row_count".into(), self.rows.len().into());
                if self.truncated {
                    obj.insert("truncated".into(), true.into());
                    obj.insert(
                        "note".into(),
                        "more rows matched than this action returns; narrow the request".into(),
                    );
                }
            }
            ActionKind::Write => {
                obj.insert("rows_affected".into(), self.rows_affected.into());
                if !self.rows.is_empty() {
                    obj.insert("returned".into(), self.rows.to_json());
                }
                if self.replayed {
                    obj.insert("replayed".into(), true.into());
                    obj.insert(
                        "note".into(),
                        "this exact write already happened; the original result is returned".into(),
                    );
                }
            }
        }
        serde_json::Value::Object(obj)
    }
}

/// Where rate limits and replay keys are kept.
///
/// Local state is per process, so two replicas enforce a limit twice over.
/// Database state is shared, at the cost of a round trip per check and a
/// fixed one-minute window instead of a sliding one.
#[derive(Debug)]
enum SharedState {
    Local {
        limiter: RateLimiter,
        replay: IdempotencyStore,
    },
    Database {
        backend: Arc<dyn Backend>,
        ttl: Duration,
    },
}

impl SharedState {
    async fn rate_ok(&self, caller: &str, action: &str, per_minute: Option<u32>) -> Result<bool> {
        let Some(limit) = per_minute else {
            return Ok(true);
        };
        match self {
            Self::Local { limiter, .. } => Ok(limiter.check(caller, action, Some(limit))),
            Self::Database { backend, .. } => backend.rate_check(caller, action, limit).await,
        }
    }

    async fn replay_get(&self, key: &str) -> Result<Option<serde_json::Value>> {
        match self {
            Self::Local { replay, .. } => Ok(replay.get(key)),
            Self::Database { backend, ttl } => backend.replay_get(key, *ttl).await,
        }
    }

    async fn replay_put(
        &self,
        key: &str,
        action: &str,
        response: &serde_json::Value,
    ) -> Result<()> {
        match self {
            Self::Local { replay, .. } => replay.put(key, action, response),
            Self::Database { backend, ttl } => {
                backend.replay_put(key, action, response, *ttl).await
            }
        }
    }
}

/// A configured, validated deployment.
#[derive(Debug)]
pub struct Engine {
    config: Config,
    registries: BTreeMap<String, Registry>,
    backend: Arc<dyn Backend>,
    schema: Schema,
    audit: AuditLog,
    approvals: ApprovalStore,
    state: SharedState,
}

impl Engine {
    /// Connect the backend, validate every action and open the stores.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear startup: backend, schema, every bundle, then the stores"
    )]
    pub async fn build(config: Config) -> Result<(Self, Vec<Warning>)> {
        let backend: Arc<dyn Backend> = match &config.backend {
            BackendConfig::Memory { fixtures } => Arc::new(MemoryBackend::from_file(fixtures)?),
            #[cfg(feature = "postgres")]
            BackendConfig::Postgres {
                dsn,
                max_connections,
                min_connections,
                schemas,
                statement_timeout,
            } => Arc::new(
                portcullis_db::PostgresBackend::connect(&portcullis_db::postgres::PgConfig {
                    dsn: dsn.clone(),
                    max_connections: *max_connections,
                    min_connections: *min_connections,
                    acquire_timeout: Duration::from_secs(10),
                    schemas: schemas.clone(),
                    statement_timeout: *statement_timeout,
                })
                .await?,
            ),
            #[cfg(feature = "mysql")]
            BackendConfig::MySql {
                dsn,
                max_connections,
                min_connections,
                schemas,
                statement_timeout,
            } => Arc::new(
                portcullis_db::MySqlBackend::connect(&portcullis_db::mysql::MySqlConfig {
                    dsn: dsn.clone(),
                    max_connections: *max_connections,
                    min_connections: *min_connections,
                    acquire_timeout: Duration::from_secs(10),
                    schemas: schemas.clone(),
                    statement_timeout: *statement_timeout,
                })
                .await?,
            ),
            #[cfg(feature = "mssql")]
            BackendConfig::MsSql {
                dsn,
                max_connections,
                schemas,
            } => Arc::new(
                // Boxed: the tiberius connect future is large enough that
                // holding it inline pushes `build` past clippy's threshold,
                // and `build` is awaited from every command.
                Box::pin(portcullis_db::MsSqlBackend::connect(
                    &portcullis_db::mssql::MsSqlConfig {
                        dsn: dsn.clone(),
                        max_connections: *max_connections,
                        acquire_timeout: Duration::from_secs(10),
                        schemas: schemas.clone(),
                    },
                ))
                .await?,
            ),
            #[cfg(not(feature = "mssql"))]
            BackendConfig::MsSql { .. } => {
                return Err(Error::Config(
                    "this build has no SQL Server support; rebuild with the `mssql` feature".into(),
                ));
            }
            #[cfg(not(feature = "mysql"))]
            BackendConfig::MySql { .. } => {
                return Err(Error::Config(
                    "this build has no MySQL support; rebuild with the `mysql` feature".into(),
                ));
            }
            #[cfg(not(feature = "postgres"))]
            BackendConfig::Postgres { .. } => {
                return Err(Error::Config(
                    "this build has no PostgreSQL support; rebuild with the `postgres` feature"
                        .into(),
                ));
            }
        };

        let schema = backend.schema().await?;

        // Every loaded version is validated, not only the live one: a canary
        // that cannot start should be found before it is promoted, not after.
        let mut registries = BTreeMap::new();
        let mut warnings = Vec::new();
        for version in config.versions() {
            let (registry, mut w) =
                Registry::build(&config, &schema, &version).map_err(|e| match e {
                    Error::Validation { action, problem } => Error::Validation {
                        action: format!("{}@{version} {action}", config.bundle_name),
                        problem,
                    },
                    other => other,
                })?;
            warnings.append(&mut w);
            registries.insert(version, registry);
        }
        let audit = AuditLog::open(&config.audit.path, config.audit.fsync, config.audit.rotate)?;
        let approvals = ApprovalStore::open(&config.approvals.path, config.approvals.ttl)?;
        let state = match config.limits.store {
            crate::config::LimitStore::Local => SharedState::Local {
                limiter: RateLimiter::new(),
                replay: IdempotencyStore::open(
                    config.audit.path.with_extension("idempotency.jsonl"),
                    config.limits.idempotency_ttl,
                )?,
            },
            crate::config::LimitStore::Database => {
                // Fail here rather than at the first write: a deployment that
                // asked for shared limits and silently got per-process ones is
                // worse than one that refuses to start.
                if !backend.supports_shared_state().await? {
                    return Err(Error::Config(
                        "[limits] store = \"database\" needs the shared-state tables; run examples/shared-state-postgres.sql or examples/shared-state-mysql.sql, or set store = \"local\"".into(),
                    ));
                }
                SharedState::Database {
                    backend: Arc::clone(&backend),
                    ttl: config.limits.idempotency_ttl,
                }
            }
        };

        Ok((
            Self {
                config,
                registries,
                backend,
                schema,
                audit,
                approvals,
                state,
            },
            warnings,
        ))
    }

    /// The actions in the default version.
    ///
    /// For tools that are not serving a caller: listing, replay, diagnostics.
    pub fn registry(&self) -> &Registry {
        self.registries
            .get(self.default_version())
            .or_else(|| self.registries.values().next())
            .expect("a deployment always has at least one bundle version")
    }

    /// The version this caller runs against.
    pub fn version_for(&self, caller: &Caller) -> String {
        let pinned = self
            .config
            .role(&caller.role)
            .and_then(|r| r.bundle.as_deref());
        self.config.routing.resolve(pinned, &caller.id)
    }

    /// The actions this caller can see.
    pub fn registry_for(&self, caller: &Caller) -> &Registry {
        let version = self.version_for(caller);
        self.registries
            .get(&version)
            .unwrap_or_else(|| self.registry())
    }

    /// Every loaded version, sorted.
    pub fn versions(&self) -> Vec<String> {
        self.registries.keys().cloned().collect()
    }

    /// The bundle name this deployment serves.
    pub fn bundle_name(&self) -> &str {
        &self.config.bundle_name
    }

    fn default_version(&self) -> &str {
        let routing = &self.config.routing;
        routing
            .aliases
            .get(&routing.default)
            .map_or(routing.default.as_str(), String::as_str)
    }

    /// The loaded configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The live schema as read at startup.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The parked-call store.
    pub fn approvals(&self) -> &ApprovalStore {
        &self.approvals
    }

    /// The backend, for tools that read the database directly.
    pub fn backend(&self) -> Arc<dyn Backend> {
        Arc::clone(&self.backend)
    }

    /// A one-line description of the backend, with no credentials in it.
    pub fn backend_description(&self) -> String {
        self.backend.describe()
    }

    /// Check the backend is reachable.
    pub async fn health(&self) -> Result<()> {
        self.backend.health().await
    }

    /// Flush the audit log to disk.
    pub fn flush_audit(&self) -> Result<()> {
        self.audit.flush()
    }

    /// Close the current audit segment and start a new one.
    pub fn rotate_audit(&self) -> Result<()> {
        self.audit.rotate_now()
    }

    /// Build a caller for a configured role.
    pub fn caller(&self, role: &str, id: &str) -> Result<Caller> {
        let r = self.config.role(role).ok_or_else(|| {
            let names: Vec<&str> = self.config.roles.keys().map(String::as_str).collect();
            let hint = did_you_mean(role, &names)
                .map_or_else(String::new, |s| format!("; did you mean `{s}`?"));
            Error::Config(format!("no role named `{role}`{hint}"))
        })?;
        Ok(Caller {
            id: id.to_owned(),
            role: r.name.clone(),
            attributes: r.attributes.clone(),
        })
    }

    /// Call an action.
    pub async fn call(
        &self,
        action_name: &str,
        args: &serde_json::Value,
        caller: &Caller,
    ) -> Result<CallResult> {
        self.dispatch(action_name, args, caller, None).await
    }

    /// Release a parked call and run it.
    pub async fn approve(&self, id: &str, approver: &Caller) -> Result<CallResult> {
        let pending = self
            .approvals
            .get(id)
            .ok_or_else(|| Error::Approval(format!("no approval request `{id}`")))?;
        self.may_decide(&pending, approver)?;
        let approval = self.approvals.claim(id, &approver.id, Status::Executed)?;
        let caller = approval.caller.clone();
        let result = self
            .dispatch(&approval.action, &approval.args, &caller, Some(&approval))
            .await;
        if let Ok(done) = &result {
            // The agent that raised this is not listening; leave the outcome
            // where it can collect it.
            let _ = self.approvals.record_result(id, &done.to_json());
        }
        if result.is_err() {
            // The claim already consumed the request; record why it failed so
            // the log does not simply show an executed approval with no effect.
            self.record(
                &approval.action,
                &caller,
                &serde_json::json!({}),
                Decision::Failed,
                0,
                Duration::ZERO,
                result.as_ref().err().map(|e| e.code().to_owned()),
                Some(id.to_owned()),
                &uuid::Uuid::new_v4().to_string(),
            );
        }
        result
    }

    /// Refuse a parked call.
    pub fn deny(&self, id: &str, approver: &Caller) -> Result<Approval> {
        let pending = self
            .approvals
            .get(id)
            .ok_or_else(|| Error::Approval(format!("no approval request `{id}`")))?;
        self.may_decide(&pending, approver)?;
        let approval = self.approvals.claim(id, &approver.id, Status::Denied)?;
        self.record(
            &approval.action,
            &approval.caller,
            &approval.args,
            Decision::Denied,
            0,
            Duration::ZERO,
            Some("approval_denied".to_owned()),
            Some(id.to_owned()),
            &uuid::Uuid::new_v4().to_string(),
        );
        Ok(approval)
    }

    /// Report on a parked call to the caller that raised it.
    ///
    /// Visible to the requester and to anyone who could decide it, and to
    /// nobody else: a request id is not a capability, but the arguments inside
    /// it may be sensitive.
    fn answer_status(&self, args: &serde_json::Value, caller: &Caller) -> Result<Rows> {
        let id = args
            .get("request")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::BadArgument {
                param: "request".into(),
                problem: "is required; it is the id returned when the call was parked".into(),
            })?;

        let approval = self
            .approvals
            .get(id)
            .ok_or_else(|| Error::Approval(format!("no approval request `{id}`")))?;

        let mine = approval.caller.id == caller.id;
        if !mine && self.may_decide(&approval, caller).is_err() {
            // Same answer as a request that does not exist, so the id cannot
            // be used to probe for other people's calls.
            return Err(Error::Approval(format!("no approval request `{id}`")));
        }

        let status = match approval.status {
            Status::Pending if approval.is_expired(self.config.approvals.ttl) => "expired",
            Status::Pending => "pending",
            Status::Executed => "executed",
            Status::Denied => "denied",
            Status::Expired => "expired",
        };

        let mut columns = vec![
            "request".to_string(),
            "action".to_string(),
            "status".to_string(),
            "raised".to_string(),
        ];
        let mut row = vec![
            Value::Text(approval.id.clone()),
            Value::Text(approval.action.clone()),
            Value::Text(status.to_owned()),
            Value::Text(approval.created.clone()),
        ];
        if let Some(decided) = &approval.decided {
            columns.push("decided".to_string());
            row.push(Value::Text(decided.clone()));
        }
        if let Some(by) = &approval.decided_by {
            columns.push("decided_by".to_string());
            row.push(Value::Text(by.clone()));
        }
        if let Some(result) = &approval.result {
            columns.push("result".to_string());
            row.push(Value::Json(result.clone()));
        }

        Ok(Rows {
            columns,
            rows: vec![row],
        })
    }

    /// May this caller decide that request?
    ///
    /// Two separate rules. A deployment can restrict approving to named roles,
    /// and — unless it says otherwise — the person who raised a request cannot
    /// clear it themselves, because a gate the requester can open is not a
    /// gate.
    pub fn may_decide(&self, approval: &Approval, approver: &Caller) -> Result<()> {
        let policy = &self.config.approvals;
        if !policy.approver_roles.is_empty()
            && !policy.approver_roles.iter().any(|r| r == &approver.role)
        {
            return Err(Error::Denied {
                role: approver.role.clone(),
                action: format!("approving `{}`", approval.action),
            });
        }
        if !policy.allow_self_approval && approval.caller.id == approver.id {
            return Err(Error::Approval(format!(
                "`{}` raised request {} and may not decide it",
                approver.id, approval.id
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn dispatch(
        &self,
        action_name: &str,
        args: &serde_json::Value,
        caller: &Caller,
        approved: Option<&Approval>,
    ) -> Result<CallResult> {
        let started = Instant::now();
        let request_id = uuid::Uuid::new_v4().to_string();
        let version = self.version_for(caller);
        let registry = self
            .registries
            .get(&version)
            .unwrap_or_else(|| self.registry());

        if action_name == STATUS_ACTION {
            let answer = self.answer_status(args, caller);
            self.record_in(
                Some(&version),
                action_name,
                caller,
                args,
                if answer.is_ok() {
                    Decision::Allowed
                } else {
                    Decision::Denied
                },
                0,
                started.elapsed(),
                answer.as_ref().err().map(|e| e.code().to_owned()),
                args.get("request")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned),
                &request_id,
            );
            return answer.map(|rows| CallResult {
                action: action_name.to_owned(),
                kind: ActionKind::Read,
                request_id,
                rows,
                rows_affected: 0,
                truncated: false,
                replayed: false,
                duration: started.elapsed(),
            });
        }

        let Some(action) = registry.get(action_name) else {
            let names = registry.names();
            let hint = did_you_mean(action_name, &names)
                .map_or_else(String::new, |s| format!("; did you mean `{s}`?"));
            self.record_in(
                Some(&version),
                action_name,
                caller,
                args,
                Decision::Denied,
                0,
                started.elapsed(),
                Some("unknown_action".into()),
                None,
                &request_id,
            );
            return Err(Error::UnknownAction(format!("{action_name}{hint}")));
        };

        // Authorisation. An approved call still has to be allowed: a role that
        // lost the permission between request and release does not get it back.
        let role = self.config.role(&caller.role);
        if !role.is_some_and(|r| r.allows(action_name)) {
            self.deny_and_record(action, caller, args, started, &request_id, "role");
            return Err(Error::Denied {
                role: caller.role.clone(),
                action: action_name.to_owned(),
            });
        }

        if !self
            .state
            .rate_ok(&caller.id, action_name, action.spec.rate_limit)
            .await
            .unwrap_or(true)
        {
            self.record_in(
                Some(&version),
                action_name,
                caller,
                args,
                Decision::Denied,
                0,
                started.elapsed(),
                Some("rate_limited".into()),
                None,
                &request_id,
            );
            return Err(Error::LimitExceeded(format!(
                "`{action_name}` is limited to {} calls per minute",
                action.spec.rate_limit.unwrap_or_default()
            )));
        }

        let values = match self.check_arguments(action, args) {
            Ok(v) => v,
            Err(e) => {
                self.record_in(
                    Some(&version),
                    action_name,
                    caller,
                    args,
                    Decision::Failed,
                    0,
                    started.elapsed(),
                    Some(e.code().to_owned()),
                    None,
                    &request_id,
                );
                return Err(e);
            }
        };

        // Approval gate.
        if approved.is_none() {
            if let Some(reason) = needs_approval(action, &values) {
                let request = self
                    .approvals
                    .create(action_name, args.clone(), caller, &reason)?;
                self.record_in(
                    Some(&version),
                    action_name,
                    caller,
                    args,
                    Decision::Pending,
                    0,
                    started.elapsed(),
                    None,
                    Some(request.id.clone()),
                    &request_id,
                );
                return Err(Error::ApprovalRequired {
                    action: action_name.to_owned(),
                    request: request.id,
                });
            }
        }

        // Replay protection for writes.
        let idem_key = idempotency_key(action, caller, &values);
        if let Some(key) = &idem_key {
            if let Some(previous) = self.state.replay_get(key).await? {
                self.record_in(
                    Some(&version),
                    action_name,
                    caller,
                    args,
                    Decision::Allowed,
                    0,
                    started.elapsed(),
                    Some("replayed".into()),
                    approved.map(|a| a.id.clone()),
                    &request_id,
                );
                return Ok(CallResult {
                    action: action_name.to_owned(),
                    kind: action.spec.kind,
                    request_id,
                    rows: rows_from_json(&previous),
                    rows_affected: 0,
                    truncated: false,
                    replayed: true,
                    duration: started.elapsed(),
                });
            }
        }

        let ctx = ExecCtx {
            args: &values,
            caller,
            timeout: action.spec.timeout,
        };
        let limit = action.limit(self.config.limits.max_rows);

        let outcome = match action.spec.kind {
            ActionKind::Read => {
                // Ask for one more row than the caller may have. If it comes
                // back, the result was cut short and the model is told so
                // rather than quietly reasoning over a partial answer.
                self.backend
                    .read(
                        &ReadPlan {
                            table: &action.table,
                            columns: &action.spec.returns,
                            filter: action.filter.as_ref(),
                            row_filter: action.row_filter.as_ref(),
                            order_by: &action.spec.order_by,
                            limit: limit.saturating_add(1),
                        },
                        &ctx,
                    )
                    .await
                    .map(|rows| (rows, 0u64))
            }
            ActionKind::Write => {
                let write = action
                    .spec
                    .write
                    .as_ref()
                    .expect("a write action always has a write block");
                self.backend
                    .write(
                        &WritePlan {
                            table: &action.table,
                            mode: write.mode,
                            columns: &action.write_columns,
                            keys: &write.keys,
                            returning: &write.returning,
                            row_filter: action.row_filter.as_ref(),
                        },
                        &ctx,
                    )
                    .await
                    .map(|o| (o.returned, o.rows_affected))
            }
        };

        let (rows, rows_affected) = match outcome {
            Ok(v) => v,
            Err(e) => {
                self.record_in(
                    Some(&version),
                    action_name,
                    caller,
                    args,
                    Decision::Failed,
                    0,
                    started.elapsed(),
                    Some(e.code().to_owned()),
                    approved.map(|a| a.id.clone()),
                    &request_id,
                );
                return Err(e);
            }
        };

        let mut rows = rows;
        let truncated =
            action.spec.kind == ActionKind::Read && rows.len() as u64 > u64::from(limit);
        if truncated {
            rows.rows.truncate(limit as usize);
        }
        let rows = self.mask_rows(action, rows);

        let result = CallResult {
            action: action_name.to_owned(),
            kind: action.spec.kind,
            request_id: request_id.clone(),
            rows,
            rows_affected,
            truncated,
            replayed: false,
            duration: started.elapsed(),
        };

        if let Some(key) = &idem_key {
            self.state
                .replay_put(key, action_name, &result.rows.to_json())
                .await?;
        }

        self.record_in(
            Some(&version),
            action_name,
            caller,
            args,
            Decision::Allowed,
            if action.spec.kind == ActionKind::Read {
                result.rows.len() as u64
            } else {
                rows_affected
            },
            result.duration,
            None,
            approved.map(|a| a.id.clone()),
            &request_id,
        );

        Ok(result)
    }

    /// Type-check the incoming arguments against the action's parameters.
    fn check_arguments(
        &self,
        action: &Action,
        args: &serde_json::Value,
    ) -> Result<BTreeMap<String, Value>> {
        let object = args.as_object().ok_or_else(|| Error::BadArgument {
            param: "arguments".into(),
            problem: "must be a JSON object".into(),
        })?;

        let size = args.to_string().len();
        if size > self.config.limits.max_request_bytes {
            return Err(Error::LimitExceeded(format!(
                "arguments are {size} bytes; this deployment accepts {}",
                self.config.limits.max_request_bytes
            )));
        }

        for key in object.keys() {
            if !action.spec.params.contains_key(key) {
                let names: Vec<&str> = action.spec.params.keys().map(String::as_str).collect();
                let hint = did_you_mean(key, &names)
                    .map_or_else(String::new, |s| format!("; did you mean `{s}`?"));
                return Err(Error::BadArgument {
                    param: key.clone(),
                    problem: format!("is not a parameter of `{}`{hint}", action.spec.name),
                });
            }
        }

        let mut values = BTreeMap::new();
        for (name, spec) in &action.spec.params {
            match object.get(name) {
                None | Some(serde_json::Value::Null) if spec.required => {
                    return Err(Error::BadArgument {
                        param: name.clone(),
                        problem: "is required".into(),
                    });
                }
                None => {}
                Some(json) => {
                    let value = coerce_argument(name, spec, json)?;
                    if !value.is_null() {
                        values.insert(name.clone(), value);
                    }
                }
            }
        }
        Ok(values)
    }

    fn mask_rows(&self, action: &Action, mut rows: Rows) -> Rows {
        if action.spec.mask.is_empty() {
            return rows;
        }
        let masks: Vec<Option<Mask>> = rows
            .columns
            .iter()
            .map(|c| {
                action
                    .spec
                    .mask
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(c))
                    .map(|(_, m)| *m)
            })
            .collect();
        for row in &mut rows.rows {
            for (value, mask) in row.iter_mut().zip(&masks) {
                if let Some(m) = mask {
                    *value = m.apply(value, &self.config.server.mask_salt);
                }
            }
        }
        rows
    }

    fn deny_and_record(
        &self,
        action: &Action,
        caller: &Caller,
        args: &serde_json::Value,
        started: Instant,
        request_id: &str,
        _why: &str,
    ) {
        self.record(
            &action.spec.name,
            caller,
            args,
            Decision::Denied,
            0,
            started.elapsed(),
            Some("denied".into()),
            None,
            request_id,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_in(
        &self,
        bundle: Option<&str>,
        action: &str,
        caller: &Caller,
        args: &serde_json::Value,
        decision: Decision,
        rows: u64,
        duration: Duration,
        error: Option<String>,
        approval: Option<String>,
        request_id: &str,
    ) {
        self.audit.append(AuditRecord {
            seq: 0,
            ts: jiff::Timestamp::now().to_string(),
            request_id: request_id.to_owned(),
            caller: caller.id.clone(),
            role: caller.role.clone(),
            action: action.to_owned(),
            params: self.mask_arguments(action, args),
            decision,
            rows,
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            error,
            approval,
            scope: caller.attributes.clone(),
            bundle: bundle.map(ToOwned::to_owned),
            prev: String::new(),
            hash: String::new(),
        });
    }

    /// Record without a bundle, for decisions made before one is resolved.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        action: &str,
        caller: &Caller,
        args: &serde_json::Value,
        decision: Decision,
        rows: u64,
        duration: Duration,
        error: Option<String>,
        approval: Option<String>,
        request_id: &str,
    ) {
        self.record_in(
            None, action, caller, args, decision, rows, duration, error, approval, request_id,
        );
    }

    /// Apply the action's column masks to the recorded arguments.
    ///
    /// A masked column would otherwise arrive in the log in clear text as soon
    /// as someone filtered on it.
    fn mask_arguments(&self, action: &str, args: &serde_json::Value) -> serde_json::Value {
        // Any version that defines the action will do: a mask is about the
        // column, and a version that renamed it will simply not match.
        let Some(spec) = self.registries.values().find_map(|r| r.get(action)) else {
            return args.clone();
        };
        if spec.spec.mask.is_empty() {
            return args.clone();
        }
        let Some(object) = args.as_object() else {
            return args.clone();
        };
        let mut out = serde_json::Map::new();
        for (key, value) in object {
            let mask = spec
                .spec
                .mask
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, m)| *m);
            out.insert(
                key.clone(),
                match (mask, value.as_str()) {
                    (Some(m), Some(text)) => m
                        .apply(&Value::Text(text.to_owned()), &self.config.server.mask_salt)
                        .to_json(),
                    _ => value.clone(),
                },
            );
        }
        serde_json::Value::Object(out)
    }
}

/// Does this call need a human?
fn needs_approval(action: &Action, values: &BTreeMap<String, Value>) -> Option<String> {
    let rule = action.spec.approval.as_ref()?;
    if rule.always {
        return Some("this action always requires approval".to_owned());
    }
    let threshold = rule.over.as_ref()?;
    let value = values.get(&threshold.param)?;
    let amount = value.as_decimal()?;
    (amount > threshold.amount).then(|| {
        format!(
            "{} is {amount}, above the {} that may run unattended",
            threshold.param, threshold.amount
        )
    })
}

/// The replay key for a write, if the action declares one.
fn idempotency_key(
    action: &Action,
    caller: &Caller,
    values: &BTreeMap<String, Value>,
) -> Option<String> {
    let write = action.spec.write.as_ref()?;
    if write.idempotency.is_empty() {
        return None;
    }
    let params: Vec<(&str, &Value)> = write
        .idempotency
        .iter()
        .filter_map(|p| values.get(p).map(|v| (p.as_str(), v)))
        .collect();
    if params.len() != write.idempotency.len() {
        // An absent key parameter means the call is not identifiable; run it.
        return None;
    }
    Some(IdempotencyStore::key(
        &action.spec.name,
        &caller.id,
        &params,
    ))
}

/// Rebuild a row set from a stored JSON response.
fn rows_from_json(json: &serde_json::Value) -> Rows {
    let Some(array) = json.as_array() else {
        return Rows::default();
    };
    let mut columns: Vec<String> = Vec::new();
    for item in array {
        if let Some(obj) = item.as_object() {
            for key in obj.keys() {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let rows = array
        .iter()
        .map(|item| {
            columns
                .iter()
                .map(|c| item.get(c).map_or(Value::Null, scalar_from_json))
                .collect()
        })
        .collect();
    Rows { columns, rows }
}

/// Rebuild a stored scalar without its JSON quoting.
///
/// The column's declared type is not available here, so a replayed decimal
/// comes back as the text it was stored as. That is the same character
/// sequence the caller saw the first time, which is what a replay promises.
fn scalar_from_json(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map_or_else(|| n.as_f64().map_or(Value::Null, Value::Float), Value::Int),
        other => Value::Json(other.clone()),
    }
}
