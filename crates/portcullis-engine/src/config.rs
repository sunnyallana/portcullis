//! Configuration loading.
//!
//! One TOML file describes the server, the backend, the roles and the actions.
//! Unknown keys are refused rather than ignored, because a silently ignored
//! `max_row` is a row limit that never applies.
//!
//! Secrets are never written in the file. Any string field marked as a secret
//! accepts `env:NAME` or `file:/path`, and the value is read at startup.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use portcullis_core::audit::{Fsync, Rotate};
use portcullis_core::branding;
use portcullis_core::spec::RawAction;
use portcullis_core::{ActionSpec, Error, Result, Value};
use serde::Deserialize;

/// A whole deployment, as written in the file.
#[derive(Debug, Clone)]
pub struct Config {
    /// Server identity and defaults.
    pub server: ServerConfig,
    /// Where the data lives.
    pub backend: BackendConfig,
    /// Audit log settings.
    pub audit: AuditConfig,
    /// Approval store settings.
    pub approvals: ApprovalsConfig,
    /// Ceilings that apply to every action.
    pub limits: Limits,
    /// HTTP transport settings, when `[http]` is present.
    pub http: Option<HttpSettings>,
    /// How HTTP callers are identified.
    pub auth: AuthSettings,
    /// Roles, keyed by name.
    pub roles: BTreeMap<String, Role>,
    /// Actions, in file order.
    pub actions: IndexMap<String, ActionSpec>,
    /// Directory the file lived in, used to resolve relative paths.
    pub base_dir: PathBuf,
}

/// Server identity and defaults.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Name reported over MCP.
    pub name: String,
    /// Salt for `hash` masking. Changing it changes every digest.
    pub mask_salt: String,
}

/// Where the data lives.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    /// PostgreSQL.
    Postgres {
        /// Resolved connection string.
        dsn: String,
        /// Pool ceiling.
        max_connections: u32,
        /// Warm connections.
        min_connections: u32,
        /// Schemas to expose; empty means all non-system schemas.
        schemas: Vec<String>,
        /// Server-side statement timeout backstop.
        statement_timeout: Duration,
    },
    /// MySQL or MariaDB.
    MySql {
        /// Resolved connection string.
        dsn: String,
        /// Pool ceiling.
        max_connections: u32,
        /// Warm connections.
        min_connections: u32,
        /// Databases to expose; empty means the one in the connection string.
        schemas: Vec<String>,
        /// Server-side statement timeout backstop.
        statement_timeout: Duration,
    },
    /// In-memory fixture, for `portcullis demo` and tests.
    Memory {
        /// Path to the fixture file.
        fixtures: PathBuf,
    },
}

/// Audit log settings.
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Where the log is written.
    pub path: PathBuf,
    /// Durability policy.
    pub fsync: Fsync,
    /// When to close a segment and start a new one.
    pub rotate: Rotate,
}

/// Approval store settings.
#[derive(Debug, Clone)]
pub struct ApprovalsConfig {
    /// Where pending requests are kept.
    pub path: PathBuf,
    /// How long a request can wait before it expires.
    pub ttl: Duration,
    /// Roles allowed to release a parked call. Empty means any role.
    pub approver_roles: Vec<String>,
    /// Whether the caller who raised a request may release it themselves.
    ///
    /// False by default: an approval gate that the requester can clear is
    /// decoration.
    pub allow_self_approval: bool,
}

/// Where rate-limit counters and replay keys live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LimitStore {
    /// In this process. Fast, and enforced per replica rather than per
    /// deployment.
    #[default]
    Local,
    /// In the database, shared by every replica. Needs the tables in
    /// `examples/shared-state-*.sql` and a backend that supports them.
    Database,
}

/// Ceilings that apply to every action.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// No action may return more rows than this, whatever it asks for.
    pub max_rows: u32,
    /// Largest accepted argument object, in bytes.
    pub max_request_bytes: usize,
    /// How long a completed write is remembered for idempotency.
    pub idempotency_ttl: Duration,
    /// Whether limits are enforced per process or across the deployment.
    pub store: LimitStore,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rows: 1_000,
            max_request_bytes: 64 * 1024,
            idempotency_ttl: Duration::from_secs(24 * 60 * 60),
            store: LimitStore::Local,
        }
    }
}

/// HTTP transport settings.
#[derive(Debug, Clone)]
pub struct HttpSettings {
    /// Address to bind, as written in the file.
    pub listen: String,
    /// Serve the approvals console.
    pub console: bool,
    /// Largest accepted request body.
    pub max_body_bytes: usize,
    /// Deadline for one request.
    pub request_timeout: Duration,
}

/// One issued API key, stored as a digest.
#[derive(Debug, Clone)]
pub struct ApiKeySettings {
    /// Lowercase hex SHA-256 of the key.
    pub hash: String,
    /// Role the key acts as.
    pub role: String,
    /// Identity recorded in the audit log.
    pub caller: String,
    /// Attributes the key carries.
    pub attributes: BTreeMap<String, Value>,
}

/// How HTTP callers prove who they are.
#[derive(Debug, Clone)]
pub enum AuthSettings {
    /// Everyone is one fixed identity. Development only.
    Open {
        /// Role every caller acts as.
        role: String,
        /// Identity recorded in the audit log.
        caller: String,
    },
    /// Bearer tokens matched against issued keys.
    ApiKey {
        /// The issued keys.
        keys: Vec<ApiKeySettings>,
    },
    /// OIDC access tokens.
    Oidc {
        /// Expected `iss`.
        issuer: String,
        /// Accepted `aud` values.
        audience: Vec<String>,
        /// JWKS endpoint. Discovered from the issuer when absent.
        jwks_url: Option<String>,
        /// Claim holding the role.
        role_claim: String,
        /// Claim holding the caller identity.
        caller_claim: String,
        /// Attribute name to claim name.
        attribute_claims: BTreeMap<String, String>,
        /// Claim value to deployment role.
        role_map: BTreeMap<String, String>,
        /// Clock skew allowance.
        leeway: Duration,
        /// How long a fetched key set is reused.
        refresh_interval: Duration,
    },
}

impl AuthSettings {
    /// True when nothing is actually verified.
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    /// One line for diagnostics.
    pub fn describe(&self) -> String {
        match self {
            Self::Open { role, .. } => format!("none (everyone is `{role}`)"),
            Self::ApiKey { keys } => format!("{} API key(s)", keys.len()),
            Self::Oidc { issuer, .. } => format!("OIDC ({issuer})"),
        }
    }
}

/// One role and what it may do.
#[derive(Debug, Clone)]
pub struct Role {
    /// Role name.
    pub name: String,
    /// Actions this role may call. `["*"]` allows every action.
    pub allow: Vec<String>,
    /// Attributes available to row filters as `$caller.*`.
    pub attributes: BTreeMap<String, Value>,
}

impl Role {
    /// May this role call the named action?
    pub fn allows(&self, action: &str) -> bool {
        self.allow.iter().any(|a| a == "*" || a == action)
    }
}

impl Config {
    /// Read and parse a configuration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read `{}`: {e}", path.display())))?;
        let base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        Self::parse(&text, &base_dir).map_err(|e| match e {
            // Prefix the file name so a multi-file deployment says which one.
            Error::Config(msg) => Error::Config(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    /// Parse configuration text.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear pass over the file's sections, each with its own message"
    )]
    pub fn parse(text: &str, base_dir: &Path) -> Result<Self> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| Error::Config(describe_toml(&e)))?;

        let mask_salt = resolve_secret(raw.server.mask_salt.as_deref(), base_dir)?
            .unwrap_or_else(|| raw.server.name.clone());

        let backend = match raw.backend.kind.as_str() {
            "postgres" => BackendConfig::Postgres {
                dsn: resolve_secret(raw.backend.dsn.as_deref(), base_dir)?.ok_or_else(|| {
                    Error::Config(
                        "[backend] kind = \"postgres\" needs a `dsn`; use `env:DATABASE_URL` to keep it out of the file".into(),
                    )
                })?,
                max_connections: raw.backend.max_connections.unwrap_or(10),
                min_connections: raw.backend.min_connections.unwrap_or(1),
                schemas: raw.backend.schemas,
                statement_timeout: Duration::from_secs(raw.backend.statement_timeout_secs.unwrap_or(60)),
            },
            "mysql" | "mariadb" => BackendConfig::MySql {
                dsn: resolve_secret(raw.backend.dsn.as_deref(), base_dir)?.ok_or_else(|| {
                    Error::Config(
                        "[backend] kind = \"mysql\" needs a `dsn`; use `env:DATABASE_URL` to keep it out of the file".into(),
                    )
                })?,
                max_connections: raw.backend.max_connections.unwrap_or(10),
                min_connections: raw.backend.min_connections.unwrap_or(1),
                schemas: raw.backend.schemas.clone(),
                statement_timeout: Duration::from_secs(
                    raw.backend.statement_timeout_secs.unwrap_or(60),
                ),
            },
            "memory" => BackendConfig::Memory {
                fixtures: resolve_path(
                    raw.backend.fixtures.as_deref().ok_or_else(|| {
                        Error::Config("[backend] kind = \"memory\" needs `fixtures`".into())
                    })?,
                    base_dir,
                ),
            },
            other => {
                return Err(Error::Config(format!(
                    "[backend] kind = \"{other}\" is not supported; use \"postgres\", \"mysql\" or \"memory\""
                )));
            }
        };

        let mut roles = BTreeMap::new();
        for r in raw.role {
            if roles.contains_key(&r.name) {
                return Err(Error::Config(format!("role `{}` is defined twice", r.name)));
            }
            if r.allow.is_empty() {
                return Err(Error::Config(format!(
                    "role `{}` allows nothing; remove it or list actions",
                    r.name
                )));
            }
            roles.insert(
                r.name.clone(),
                Role {
                    name: r.name,
                    allow: r.allow,
                    attributes: r.attributes,
                },
            );
        }
        if roles.is_empty() {
            return Err(Error::Config(
                "no `[[role]]` is defined, so nobody could call anything".into(),
            ));
        }

        let auth = parse_auth(raw.auth)?;

        let mut actions = IndexMap::new();
        for (name, raw_action) in raw.action {
            if !portcullis_core::spec::is_identifier(&name) {
                return Err(Error::Config(format!(
                    "action name `{name}` must be letters, digits and underscore, not starting with a digit"
                )));
            }
            let spec =
                ActionSpec::from_raw(&name, raw_action).map_err(|problem| Error::Validation {
                    action: name.clone(),
                    problem,
                })?;
            actions.insert(name, spec);
        }
        if actions.is_empty() {
            return Err(Error::Config("no `[action.*]` is defined".into()));
        }

        // Every action a role names must exist, or the operator has a typo
        // that quietly grants nothing.
        for role in roles.values() {
            for allowed in &role.allow {
                if allowed != "*" && !actions.contains_key(allowed) {
                    let names: Vec<&str> = actions.keys().map(String::as_str).collect();
                    let hint = portcullis_core::did_you_mean(allowed, &names)
                        .map_or_else(String::new, |s| format!("; did you mean `{s}`?"));
                    return Err(Error::Config(format!(
                        "role `{}` allows `{allowed}`, which is not an action{hint}",
                        role.name
                    )));
                }
            }
        }

        Ok(Self {
            server: ServerConfig {
                name: raw.server.name,
                mask_salt,
            },
            backend,
            audit: AuditConfig {
                path: resolve_path(
                    raw.audit.path.as_deref().unwrap_or(branding::AUDIT_FILE),
                    base_dir,
                ),
                fsync: raw.audit.fsync.unwrap_or_default(),
                rotate: Rotate {
                    // An explicit 0 turns rotation off; absent means the
                    // default. Some(0) would otherwise rotate every record.
                    max_bytes: match raw.audit.rotate_bytes {
                        Some(0) => None,
                        Some(bytes) => Some(bytes),
                        None => Rotate::default().max_bytes,
                    },
                    max_age: raw
                        .audit
                        .rotate_days
                        .filter(|d| *d > 0)
                        .map(|d| Duration::from_secs(d * 86_400)),
                },
            },
            approvals: ApprovalsConfig {
                path: resolve_path(
                    raw.approvals
                        .path
                        .as_deref()
                        .unwrap_or(branding::APPROVALS_FILE),
                    base_dir,
                ),
                ttl: Duration::from_secs(raw.approvals.ttl_secs.unwrap_or(7 * 24 * 60 * 60)),
                approver_roles: raw.approvals.approver_roles,
                allow_self_approval: raw.approvals.allow_self_approval,
            },
            http: raw.http.map(|h| HttpSettings {
                listen: h.listen.unwrap_or_else(|| "127.0.0.1:8080".to_owned()),
                console: h.console,
                max_body_bytes: h.max_body_bytes.unwrap_or(256 * 1024),
                request_timeout: Duration::from_secs(h.request_timeout_secs.unwrap_or(60)),
            }),
            auth,
            limits: Limits {
                max_rows: raw.limits.max_rows.unwrap_or(1_000),
                max_request_bytes: raw.limits.max_request_bytes.unwrap_or(64 * 1024),
                idempotency_ttl: Duration::from_secs(
                    raw.limits.idempotency_ttl_secs.unwrap_or(24 * 60 * 60),
                ),
                store: match raw.limits.store.as_deref() {
                    None | Some("local") => LimitStore::Local,
                    Some("database") => LimitStore::Database,
                    Some(other) => {
                        return Err(Error::Config(format!(
                            "[limits] store = \"{other}\" is not supported; use \"local\" or \"database\""
                        )));
                    }
                },
            },
            roles,
            actions,
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Look up a role.
    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }
}

/// Build the authentication settings.
///
/// With no `[auth]` block the server is open, which is only safe on loopback;
/// the HTTP entry point refuses to bind anything else in that state.
fn parse_auth(raw: Option<RawAuth>) -> Result<AuthSettings> {
    let Some(raw) = raw else {
        return Ok(AuthSettings::Open {
            role: String::new(),
            caller: "local".into(),
        });
    };
    Ok(match raw.kind.as_str() {
        "none" => AuthSettings::Open {
            role: raw.role.unwrap_or_default(),
            caller: raw.caller.unwrap_or_else(|| "local".into()),
        },
        "api_key" => {
            if raw.key.is_empty() {
                return Err(Error::Config(
                    "[auth] kind = \"api_key\" needs at least one [[auth.key]]".into(),
                ));
            }
            let mut keys = Vec::new();
            for k in raw.key {
                let hash = k.hash.trim().to_ascii_lowercase();
                if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Error::Config(format!(
                        "[[auth.key]] hash `{hash}` is not a 64-character SHA-256 digest"
                    )));
                }
                keys.push(ApiKeySettings {
                    hash,
                    role: k.role,
                    caller: k.caller,
                    attributes: k.attributes,
                });
            }
            AuthSettings::ApiKey { keys }
        }
        "oidc" => {
            let issuer = raw
                .issuer
                .ok_or_else(|| Error::Config("[auth] kind = \"oidc\" needs an `issuer`".into()))?;
            if raw.audience.is_empty() {
                return Err(Error::Config(
                    "[auth] kind = \"oidc\" needs at least one `audience`; without it a token minted for another service would be accepted".into(),
                ));
            }
            AuthSettings::Oidc {
                issuer,
                audience: raw.audience,
                jwks_url: raw.jwks_url,
                role_claim: raw
                    .role_claim
                    .unwrap_or_else(|| branding::DEFAULT_ROLE_CLAIM.into()),
                caller_claim: raw.caller_claim.unwrap_or_else(|| "sub".into()),
                attribute_claims: raw.attribute_claims,
                role_map: raw.role_map,
                leeway: Duration::from_secs(raw.leeway_secs.unwrap_or(60)),
                refresh_interval: Duration::from_secs(raw.refresh_secs.unwrap_or(300)),
            }
        }
        other => {
            return Err(Error::Config(format!(
                "[auth] kind = \"{other}\" is not supported; use \"oidc\", \"api_key\" or \"none\""
            )));
        }
    })
}

/// Resolve `env:NAME`, `file:/path` or a literal.
fn resolve_secret(spec: Option<&str>, base_dir: &Path) -> Result<Option<String>> {
    let Some(spec) = spec else { return Ok(None) };
    if let Some(var) = spec.strip_prefix("env:") {
        return std::env::var(var)
            .map(Some)
            .map_err(|_| Error::Config(format!("environment variable `{var}` is not set")));
    }
    if let Some(path) = spec.strip_prefix("file:") {
        let path = resolve_path(path, base_dir);
        return std::fs::read_to_string(&path)
            .map(|s| Some(s.trim().to_owned()))
            .map_err(|e| Error::Config(format!("cannot read `{}`: {e}", path.display())));
    }
    Ok(Some(spec.to_owned()))
}

fn resolve_path(value: &str, base_dir: &Path) -> PathBuf {
    let p = PathBuf::from(value);
    if p.is_absolute() { p } else { base_dir.join(p) }
}

/// Turn a TOML error into something an operator can act on.
fn describe_toml(e: &toml::de::Error) -> String {
    let msg = e.message().trim().to_owned();
    match e.span() {
        Some(span) => format!("{msg} (at byte {})", span.start),
        None => msg,
    }
}

// ---------------------------------------------------------------------------
// Raw shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: RawServer,
    backend: RawBackend,
    #[serde(default)]
    audit: RawAudit,
    #[serde(default)]
    approvals: RawApprovals,
    #[serde(default)]
    limits: RawLimits,
    #[serde(default)]
    http: Option<RawHttp>,
    #[serde(default)]
    auth: Option<RawAuth>,
    #[serde(default)]
    role: Vec<RawRole>,
    #[serde(default)]
    action: IndexMap<String, RawAction>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    name: String,
    #[serde(default)]
    mask_salt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackend {
    kind: String,
    #[serde(default)]
    dsn: Option<String>,
    #[serde(default)]
    fixtures: Option<String>,
    #[serde(default)]
    max_connections: Option<u32>,
    #[serde(default)]
    min_connections: Option<u32>,
    #[serde(default)]
    schemas: Vec<String>,
    #[serde(default)]
    statement_timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAudit {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    fsync: Option<Fsync>,
    /// Close a segment once it passes this many bytes. 0 disables rotation.
    #[serde(default)]
    rotate_bytes: Option<u64>,
    /// Close a segment once it has been open this many days.
    #[serde(default)]
    rotate_days: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApprovals {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    approver_roles: Vec<String>,
    #[serde(default)]
    allow_self_approval: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    #[serde(default)]
    store: Option<String>,
    #[serde(default)]
    max_rows: Option<u32>,
    #[serde(default)]
    max_request_bytes: Option<usize>,
    #[serde(default)]
    idempotency_ttl_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHttp {
    #[serde(default)]
    listen: Option<String>,
    #[serde(default = "yes")]
    console: bool,
    #[serde(default)]
    max_body_bytes: Option<usize>,
    #[serde(default)]
    request_timeout_secs: Option<u64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    kind: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    caller: Option<String>,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    audience: Vec<String>,
    #[serde(default)]
    jwks_url: Option<String>,
    #[serde(default)]
    role_claim: Option<String>,
    #[serde(default)]
    caller_claim: Option<String>,
    #[serde(default)]
    attribute_claims: BTreeMap<String, String>,
    #[serde(default)]
    role_map: BTreeMap<String, String>,
    #[serde(default)]
    leeway_secs: Option<u64>,
    #[serde(default)]
    refresh_secs: Option<u64>,
    #[serde(default)]
    key: Vec<RawApiKey>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApiKey {
    hash: String,
    role: String,
    caller: String,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRole {
    name: String,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [server]
        name = "test"
        [backend]
        kind = "memory"
        fixtures = "data.json"
        [[role]]
        name = "support"
        allow = ["find_order"]
        [action.find_order]
        description = "Find an order"
        table = "orders"
        returns = ["order_no"]
    "#;

    #[test]
    fn a_minimal_file_parses() {
        let c = Config::parse(MINIMAL, Path::new(".")).unwrap();
        assert_eq!(c.server.name, "test");
        assert_eq!(c.actions.len(), 1);
        assert!(c.role("support").unwrap().allows("find_order"));
        assert!(!c.role("support").unwrap().allows("refund_order"));
    }

    #[test]
    fn a_role_allowing_a_missing_action_is_a_typo_worth_catching() {
        let src = MINIMAL.replace("allow = [\"find_order\"]", "allow = [\"find_ordr\"]");
        let err = Config::parse(&src, Path::new(".")).unwrap_err();
        assert!(
            format!("{err}").contains("did you mean `find_order`"),
            "{err}"
        );
    }

    #[test]
    fn a_config_with_no_roles_is_refused() {
        let src = MINIMAL.replace(
            "[[role]]\n        name = \"support\"\n        allow = [\"find_order\"]",
            "",
        );
        let err = Config::parse(&src, Path::new(".")).unwrap_err();
        assert!(format!("{err}").contains("role"), "{err}");
    }

    #[test]
    fn file_secrets_are_resolved_and_trimmed() {
        let dir = std::env::temp_dir();
        let name = format!("portcullis-dsn-{}.txt", uuid::Uuid::new_v4());
        std::fs::write(dir.join(&name), "postgres://u@h/db\n").unwrap();
        let src = format!(
            r#"
            [server]
            name = "t"
            [backend]
            kind = "postgres"
            dsn = "file:{name}"
            [[role]]
            name = "r"
            allow = ["*"]
            [action.a]
            description = "d"
            table = "t"
            returns = ["c"]
        "#
        );
        let c = Config::parse(&src, &dir).unwrap();
        match c.backend {
            BackendConfig::Postgres { dsn, .. } => assert_eq!(dsn, "postgres://u@h/db"),
            other => panic!("expected postgres, got {other:?}"),
        }
        let _ = std::fs::remove_file(dir.join(&name));
    }

    #[test]
    fn a_missing_env_secret_names_the_variable() {
        let src = r#"
            [server]
            name = "t"
            [backend]
            kind = "postgres"
            dsn = "env:PORTCULLIS_DEFINITELY_NOT_SET"
            [[role]]
            name = "r"
            allow = ["*"]
            [action.a]
            description = "d"
            table = "t"
            returns = ["c"]
        "#;
        let err = Config::parse(src, Path::new(".")).unwrap_err();
        assert!(
            format!("{err}").contains("PORTCULLIS_DEFINITELY_NOT_SET"),
            "{err}"
        );
    }

    #[test]
    fn unknown_top_level_keys_are_refused() {
        let src = format!("{MINIMAL}\n[telemetry]\nenabled = true\n");
        let err = Config::parse(&src, Path::new(".")).unwrap_err();
        assert!(format!("{err}").contains("telemetry"), "{err}");
    }
}
