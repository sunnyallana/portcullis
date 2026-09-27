//! The `portcullis` command line.
//!
//! Every subcommand loads the same configuration file and, apart from `init`,
//! connects to the same backend, so what `validate` checks is what `serve`
//! publishes. Logs always go to stderr: `serve` owns stdout for the protocol.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use owo_colors::OwoColorize;
use portcullis_core::audit::Fsync;
use portcullis_core::{ActionKind, AuditLog, Caller, Error};
use portcullis_engine::{Config, Engine};

use portcullis_core::branding::{self, env as env_var};
const EXAMPLE_CONFIG: &str = include_str!("../../../examples/orders.toml");
const EXAMPLE_FIXTURE: &str = include_str!("../../../examples/demo-data.json");

/// A governed data-action layer for AI agents.
#[derive(Debug, Parser)]
#[command(name = branding::BIN, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args, Clone)]
struct Common {
    /// Path to the configuration file.
    #[arg(short, long, env = env_var::CONFIG, default_value = branding::CONFIG_FILE, global = true)]
    config: PathBuf,

    /// Role to act as. Defaults to the only role, when there is only one.
    #[arg(long, env = env_var::ROLE, global = true)]
    role: Option<String>,

    /// Caller identity recorded in the audit log.
    #[arg(long, env = env_var::CALLER, default_value = "cli", global = true)]
    caller: String,

    /// Print machine-readable JSON instead of formatted text.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a starter configuration and demo data into a directory.
    Init {
        /// Where to write the files.
        #[arg(default_value = ".")]
        directory: PathBuf,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
    },

    /// Check the configuration against the live schema.
    Validate {
        #[command(flatten)]
        common: Common,
    },

    /// Show the tools that would be published.
    Tools {
        #[command(flatten)]
        common: Common,
    },

    /// Serve MCP: over stdin and stdout, or over HTTP with --http.
    Serve {
        /// Serve over HTTP instead of stdio, using the `[http]` and `[auth]`
        /// sections. Optionally overrides the configured address.
        #[arg(long, value_name = "ADDRESS", num_args = 0..=1, default_missing_value = "")]
        http: Option<String>,
        #[command(flatten)]
        common: Common,
    },

    /// Mint an API key and print the configuration to paste.
    Apikey {
        /// Role the key will act as.
        #[arg(long)]
        role: String,
        /// Identity recorded in the audit log.
        #[arg(long, default_value = "service")]
        caller: String,
    },

    /// Call one action directly, as a role.
    Call {
        /// Action name.
        #[arg(long)]
        action: String,
        /// Argument as `name=value`. Values parse as JSON when they can.
        #[arg(long = "arg", value_name = "NAME=VALUE")]
        args: Vec<String>,
        #[command(flatten)]
        common: Common,
    },

    /// Inspect and decide parked calls.
    Approvals {
        #[command(subcommand)]
        what: ApprovalCommand,
    },

    /// Inspect the audit log.
    Audit {
        #[command(subcommand)]
        what: AuditCommand,
    },

    /// Re-run recorded read calls and report what changed.
    Replay {
        /// Write the outcomes to this file as a baseline.
        #[arg(long, value_name = "PATH")]
        record: Option<PathBuf>,
        /// Compare against a baseline recorded earlier.
        #[arg(long, value_name = "PATH")]
        against: Option<PathBuf>,
        #[command(flatten)]
        common: Common,
    },

    /// Read a database and draft a configuration for it.
    Profile {
        /// Connection string, or `env:NAME`. Omit to profile the configured backend.
        #[arg(long)]
        dsn: Option<String>,
        /// Schemas to look at. Defaults to every non-system schema.
        #[arg(long = "schema", value_name = "NAME")]
        schemas: Vec<String>,
        /// Rows to sample per table.
        #[arg(long, default_value_t = 200)]
        sample: u32,
        /// Where to write the draft. Defaults to standard output.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Name for the drafted server.
        #[arg(long, default_value = "drafted")]
        name: String,
        #[command(flatten)]
        common: Common,
    },

    /// Check configuration, connectivity and schema in one pass.
    Doctor {
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Debug, Subcommand)]
enum ApprovalCommand {
    /// List requests that are still waiting.
    List {
        #[command(flatten)]
        common: Common,
    },
    /// Approve a request and run it.
    Approve {
        /// Request identifier.
        id: String,
        #[command(flatten)]
        common: Common,
    },
    /// Refuse a request.
    Deny {
        /// Request identifier.
        id: String,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Debug, Subcommand)]
enum AuditCommand {
    /// Verify the hash chain.
    Verify {
        #[command(flatten)]
        common: Common,
    },
    /// Show the most recent entries.
    Tail {
        /// How many entries to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        #[command(flatten)]
        common: Common,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            report(&e);
            ExitCode::from(1)
        }
    }
}

/// Logs go to stderr so that `serve` can own stdout.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_env(env_var::LOG).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

async fn run(cli: Cli) -> Result<ExitCode, Error> {
    match cli.command {
        Command::Init { directory, force } => init(&directory, force),
        Command::Validate { common } => validate(&common).await,
        Command::Tools { common } => tools(&common).await,
        Command::Serve { http, common } => serve(&common, http.as_deref()).await,
        Command::Apikey { role, caller } => Ok(apikey(&role, &caller)),
        Command::Call {
            action,
            args,
            common,
        } => call(&common, &action, &args).await,
        Command::Approvals { what } => approvals(what).await,
        Command::Audit { what } => audit(what),
        Command::Doctor { common } => doctor(&common).await,
        Command::Replay {
            record,
            against,
            common,
        } => replay(&common, record.as_deref(), against.as_deref()).await,
        Command::Profile {
            dsn,
            schemas,
            sample,
            out,
            name,
            common,
        } => {
            profile(
                &common,
                dsn.as_deref(),
                &schemas,
                sample,
                out.as_deref(),
                &name,
            )
            .await
        }
    }
}

fn init(directory: &PathBuf, force: bool) -> Result<ExitCode, Error> {
    std::fs::create_dir_all(directory)?;
    let config = directory.join(branding::CONFIG_FILE);
    let fixture = directory.join("demo-data.json");

    for (path, contents) in [(&config, EXAMPLE_CONFIG), (&fixture, EXAMPLE_FIXTURE)] {
        if path.exists() && !force {
            return Err(Error::Config(format!(
                "`{}` already exists; pass --force to overwrite",
                path.display()
            )));
        }
        std::fs::write(path, contents)?;
        println!("{} {}", "created".green(), path.display());
    }

    println!();
    println!("Next:");
    let bin = branding::BIN;
    println!("  {bin} validate --config {}", config.display());
    println!(
        "  {bin} call --config {} --action find_order --arg order_no=8812 --role support_eu",
        config.display()
    );
    println!("  {bin} serve --config {}", config.display());
    Ok(ExitCode::SUCCESS)
}

async fn build(common: &Common) -> Result<(Engine, Vec<portcullis_engine::Warning>), Error> {
    let config = Config::load(&common.config)?;
    Engine::build(config).await
}

fn caller_for(engine: &Engine, common: &Common) -> Result<Caller, Error> {
    let role = if let Some(r) = &common.role {
        r.clone()
    } else {
        {
            let roles: Vec<&String> = engine.config().roles.keys().collect();
            match roles.as_slice() {
                [only] => (*only).clone(),
                _ => {
                    return Err(Error::Config(format!(
                        "this deployment has several roles ({}); pass --role",
                        roles
                            .iter()
                            .map(|r| r.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }
    };
    engine.caller(&role, &common.caller)
}

async fn validate(common: &Common) -> Result<ExitCode, Error> {
    let (engine, warnings) = build(common).await?;
    if common.json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "actions": engine.registry().len(),
                "backend": engine.backend_description(),
                "warnings": warnings.iter().map(|w| serde_json::json!({
                    "action": w.action, "message": w.message
                })).collect::<Vec<_>>(),
            })
        );
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{} {} action(s) validated against {}",
        "ok".green().bold(),
        engine.registry().len(),
        engine.backend_description()
    );
    for w in &warnings {
        println!("{} {}: {}", "warning".yellow().bold(), w.action, w.message);
    }
    if warnings.is_empty() {
        println!("no warnings");
    }
    Ok(ExitCode::SUCCESS)
}

async fn tools(common: &Common) -> Result<ExitCode, Error> {
    let (engine, _) = build(common).await?;
    if common.json {
        let tools: Vec<_> = engine
            .registry()
            .iter()
            .map(|(name, a)| {
                serde_json::json!({
                    "name": name,
                    "kind": if a.spec.kind == ActionKind::Read { "read" } else { "write" },
                    "description": a.spec.description,
                    "table": a.table,
                    "inputSchema": a.input_schema,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&tools)?);
        return Ok(ExitCode::SUCCESS);
    }

    for (name, action) in engine.registry().iter() {
        let kind = match action.spec.kind {
            ActionKind::Read => "read ".blue().to_string(),
            ActionKind::Write => "write".magenta().to_string(),
        };
        println!("{kind} {}  {}", name.bold(), action.table.dimmed());
        println!("      {}", action.spec.description);
        if !action.spec.params.is_empty() {
            let params: Vec<String> = action
                .spec
                .params
                .iter()
                .map(|(n, p)| {
                    if p.required {
                        format!("{n}: {}", p.ty)
                    } else {
                        format!("{n}?: {}", p.ty)
                    }
                })
                .collect();
            println!("      ({})", params.join(", "));
        }
        if action.row_filter.is_some() {
            println!("      {}", "scoped by row filter".dimmed());
        }
        if action.spec.approval.is_some() {
            println!("      {}", "approval gate".yellow());
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn serve(common: &Common, http: Option<&str>) -> Result<ExitCode, Error> {
    let (engine, warnings) = build(common).await?;
    for w in &warnings {
        tracing::warn!(action = %w.action, "{}", w.message);
    }
    if let Some(address) = http {
        return serve_http(engine, address).await;
    }
    let caller = caller_for(&engine, common)?;
    tracing::info!(
        backend = %engine.backend_description(),
        actions = engine.registry().len(),
        role = %caller.role,
        "serving MCP on stdio"
    );
    let engine = Arc::new(engine);
    portcullis_mcp::serve_stdio(Arc::clone(&engine), caller).await?;
    engine.flush_audit()?;
    Ok(ExitCode::SUCCESS)
}

/// Re-run recorded read calls and report what changed.
async fn replay(
    common: &Common,
    record: Option<&std::path::Path>,
    against: Option<&std::path::Path>,
) -> Result<ExitCode, Error> {
    use portcullis_engine::replay::{Baseline, Verdict, recorded_calls};

    let (engine, _) = build(common).await?;
    let (calls, skipped) = recorded_calls(&engine.config().audit.path, &engine)?;

    if calls.is_empty() {
        println!(
            "{}",
            "no replayable calls in the audit log (writes and masked calls are never replayed)"
                .dimmed()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let baseline = against.map(Baseline::load).transpose()?;
    let mut report = portcullis_engine::replay::replay(&engine, &calls, baseline.as_ref()).await?;
    report.skipped = skipped;
    engine.flush_audit()?;

    if common.json {
        let changes: Vec<_> = report
            .changes()
            .iter()
            .map(|(key, verdict)| serde_json::json!({ "call": key, "verdict": format!("{verdict:?}") }))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "replayed": report.outcomes.len(),
                "skipped": report.skipped.len(),
                "changes": changes,
                "clean": report.is_clean(),
            })
        );
    } else {
        println!(
            "replayed {} call(s), skipped {}",
            report.outcomes.len().to_string().bold(),
            report.skipped.len()
        );
        for (label, why) in &report.skipped {
            println!("  {} {label}: {}", "skip".dimmed(), why.reason().dimmed());
        }
        if against.is_some() {
            for (key, verdict) in report.changes() {
                let text = match verdict {
                    Verdict::RowsChanged { before, after } => {
                        format!("{before} row(s) before, {after} now")
                    }
                    Verdict::ContentChanged => "same count, different content".to_owned(),
                    Verdict::StatusChanged { before, after } => format!(
                        "{} before, {} now",
                        before.as_deref().unwrap_or("ok"),
                        after.as_deref().unwrap_or("ok")
                    ),
                    Verdict::Same | Verdict::New => continue,
                };
                println!("{} {key}", "changed".yellow().bold());
                println!("        {text}");
            }
            if report.is_clean() {
                println!("{}", "nothing changed".green().bold());
            }
        }
    }

    if let Some(path) = record {
        let baseline = Baseline {
            recorded: jiff::Timestamp::now().to_string(),
            outcomes: report.outcomes.clone(),
        };
        baseline.save(path)?;
        println!("{} {}", "recorded".green(), path.display());
    }

    // A difference is not an error, but it should fail a pipeline that asked.
    Ok(if against.is_some() && !report.is_clean() {
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
    })
}

/// Read a database and draft a configuration for it.
async fn profile(
    common: &Common,
    dsn: Option<&str>,
    schemas: &[String],
    sample: u32,
    out: Option<&std::path::Path>,
    name: &str,
) -> Result<ExitCode, Error> {
    use portcullis_db::Backend;

    // Profiling happens before any actions exist, so it can work from a bare
    // connection string rather than a finished configuration.
    let backend: Arc<dyn Backend> = if let Some(spec) = dsn {
        let resolved = if let Some(var) = spec.strip_prefix("env:") {
            std::env::var(var)
                .map_err(|_| Error::Config(format!("environment variable `{var}` is not set")))?
        } else {
            spec.to_owned()
        };
        connect_for_profile(&resolved, schemas).await?
    } else {
        let (engine, _) = build(common).await?;
        engine.backend()
    };

    let schema = backend.schema().await?;
    eprintln!(
        "{} {} table(s) from {}",
        "reading".dimmed(),
        schema.tables.len(),
        backend.describe()
    );
    let profile = portcullis_engine::profile::profile(backend.as_ref(), &schema, sample).await?;
    eprintln!("{} {}", "found".dimmed(), profile.summary());
    emit_profile(&profile, out, name)
}

#[cfg(feature = "postgres")]
async fn connect_for_profile(
    dsn: &str,
    schemas: &[String],
) -> Result<Arc<dyn portcullis_db::Backend>, Error> {
    Ok(Arc::new(
        portcullis_db::PostgresBackend::connect(&portcullis_db::postgres::PgConfig {
            dsn: dsn.to_owned(),
            max_connections: 2,
            min_connections: 1,
            acquire_timeout: std::time::Duration::from_secs(10),
            schemas: schemas.to_vec(),
            statement_timeout: std::time::Duration::from_secs(60),
        })
        .await?,
    ))
}

#[cfg(not(feature = "postgres"))]
async fn connect_for_profile(
    _dsn: &str,
    _schemas: &[String],
) -> Result<Arc<dyn portcullis_db::Backend>, Error> {
    Err(Error::Config(
        "this build has no PostgreSQL support; rebuild with the `postgres` feature".into(),
    ))
}

fn emit_profile(
    profile: &portcullis_engine::Profile,
    out: Option<&std::path::Path>,
    name: &str,
) -> Result<ExitCode, Error> {
    let draft = profile.to_toml(name);
    match out {
        Some(path) => {
            std::fs::write(path, &draft)?;
            println!("{} {}", "wrote".green(), path.display());
            println!();
            println!("Next:");
            println!("  review the masks and row filters, then");
            println!("  {} validate --config {}", branding::BIN, path.display());
        }
        None => print!("{draft}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Serve MCP over HTTP, with identity resolved per request.
#[allow(
    clippy::too_many_lines,
    reason = "one authenticator per kind; the kinds read better together"
)]
async fn serve_http(engine: Engine, address_override: &str) -> Result<ExitCode, Error> {
    use portcullis_engine::config::AuthSettings;
    use portcullis_http::HttpConfig;
    use portcullis_http::auth::{
        ApiKey, ApiKeyAuthenticator, Authenticator, HttpKeySource, OidcAuthenticator, OidcConfig,
        OpenAuthenticator, StaticKeySource,
    };

    let settings =
        engine
            .config()
            .http
            .clone()
            .unwrap_or(portcullis_engine::config::HttpSettings {
                listen: "127.0.0.1:8080".into(),
                console: true,
                max_body_bytes: 256 * 1024,
                request_timeout: std::time::Duration::from_secs(60),
            });
    let listen_text = if address_override.is_empty() {
        settings.listen.clone()
    } else {
        address_override.to_owned()
    };
    let listen: std::net::SocketAddr = listen_text
        .parse()
        .map_err(|e| Error::Config(format!("`{listen_text}` is not an address to bind: {e}")))?;

    let auth: Arc<dyn Authenticator> = match engine.config().auth.clone() {
        AuthSettings::Open { role, caller } => {
            // An open server reachable from off-box is not a configuration
            // anyone means to have, so it is refused rather than warned about.
            if !listen.ip().is_loopback() {
                return Err(Error::Config(format!(
                    "refusing to serve {listen} with no authentication; configure [auth] or bind 127.0.0.1"
                )));
            }
            let role = if role.is_empty() {
                let roles: Vec<&String> = engine.config().roles.keys().collect();
                match roles.as_slice() {
                    [only] => (*only).clone(),
                    _ => {
                        return Err(Error::Config(
                            "[auth] kind = \"none\" needs a `role` when the deployment has several"
                                .into(),
                        ));
                    }
                }
            } else {
                role
            };
            engine.caller(&role, &caller)?; // fail now if the role is unknown
            Arc::new(OpenAuthenticator::new(role, caller))
        }
        AuthSettings::ApiKey { keys } => Arc::new(ApiKeyAuthenticator::new(
            keys.into_iter()
                .map(|k| ApiKey {
                    hash: k.hash,
                    role: k.role,
                    caller: k.caller,
                    attributes: k.attributes,
                })
                .collect(),
        )),
        AuthSettings::Oidc {
            issuer,
            audience,
            jwks_url,
            role_claim,
            caller_claim,
            attribute_claims,
            role_map,
            leeway,
            refresh_interval,
        } => {
            let config = OidcConfig {
                issuer: issuer.clone(),
                audience,
                role_claim,
                caller_claim,
                attribute_claims,
                role_map,
                leeway,
                refresh_interval,
            };
            let source: Arc<dyn portcullis_http::auth::KeySource> = match jwks_url {
                Some(url) if url.starts_with("file:") => {
                    let path = url.trim_start_matches("file:");
                    let text = std::fs::read_to_string(path)
                        .map_err(|e| Error::Config(format!("cannot read `{path}`: {e}")))?;
                    let set = serde_json::from_str(&text)
                        .map_err(|e| Error::Config(format!("`{path}` is not a JWKS: {e}")))?;
                    Arc::new(StaticKeySource(set))
                }
                Some(url) => Arc::new(HttpKeySource::new(url).map_err(Error::Config)?),
                None => Arc::new(
                    HttpKeySource::discover(&issuer)
                        .await
                        .map_err(Error::Config)?,
                ),
            };
            let authenticator = OidcAuthenticator::new(config, source);
            // Fetch the keys now: a wrong URL should fail the boot, not the
            // first request at 3am.
            let count = authenticator.warm().await.map_err(Error::Config)?;
            tracing::info!(keys = count, issuer = %issuer, "loaded signing keys");
            Arc::new(authenticator)
        }
    };

    if engine.config().approvals.approver_roles.is_empty() {
        tracing::warn!(
            "[approvals] approver_roles is empty, so any role may release a parked call over HTTP"
        );
    }

    tracing::info!(
        backend = %engine.backend_description(),
        actions = engine.registry().len(),
        auth = %auth.describe(),
        "starting"
    );

    let config = HttpConfig {
        listen,
        console: settings.console,
        max_body_bytes: settings.max_body_bytes,
        request_timeout: settings.request_timeout,
    };
    let engine = Arc::new(engine);
    let shutdown = shutdown_signal();
    portcullis_http::serve(Arc::clone(&engine), auth, &config, shutdown).await?;
    engine.flush_audit()?;
    Ok(ExitCode::SUCCESS)
}

/// Resolve when the process is asked to stop.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("interrupted; finishing in-flight requests"),
        () = terminate => tracing::info!("terminating; finishing in-flight requests"),
    }
}

/// Mint an API key. The key is shown once; only its digest is stored.
fn apikey(role: &str, caller: &str) -> ExitCode {
    let key = format!(
        "sk_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let hash = portcullis_http::auth::ApiKeyAuthenticator::hash(&key);
    println!("{}", "Key (shown once, store it now):".bold());
    println!("  {key}");
    println!();
    println!("{}", "Add to your configuration:".bold());
    println!();
    println!("[[auth.key]]");
    println!("hash = \"{hash}\"");
    println!("role = \"{role}\"");
    println!("caller = \"{caller}\"");
    ExitCode::SUCCESS
}

async fn call(common: &Common, action: &str, args: &[String]) -> Result<ExitCode, Error> {
    let (engine, _) = build(common).await?;
    let caller = caller_for(&engine, common)?;

    // Shell arguments have no types, so read them against the types the action
    // declares. Without this, `--arg order_no=8812` would arrive as a number
    // and be rejected by a text parameter, which is correct over the wire and
    // merely annoying on a command line.
    let published = engine.registry().get(action);
    let mut object = serde_json::Map::new();
    for pair in args {
        let (name, raw) = pair.split_once('=').ok_or_else(|| Error::BadArgument {
            param: pair.clone(),
            problem: "should be written as name=value".into(),
        })?;
        let declared = published
            .and_then(|a| a.spec.params.get(name))
            .map(|p| p.ty);
        object.insert(name.to_owned(), shell_value(declared, raw));
    }

    match engine
        .call(action, &serde_json::Value::Object(object), &caller)
        .await
    {
        Ok(result) => {
            if common.json {
                println!("{}", serde_json::to_string_pretty(&result.to_json())?);
            } else {
                print_result(&result);
            }
            engine.flush_audit()?;
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            engine.flush_audit()?;
            if common.json {
                println!(
                    "{}",
                    serde_json::json!({ "error": e.code(), "message": e.to_string() })
                );
                Ok(ExitCode::from(1))
            } else {
                Err(e)
            }
        }
    }
}

/// Read one `--arg` value against the type the action declares.
fn shell_value(declared: Option<portcullis_core::DataType>, raw: &str) -> serde_json::Value {
    use portcullis_core::DataType as T;
    match declared {
        // Text-shaped types take the characters as written, so an order number
        // that happens to be all digits stays a string.
        Some(T::Text | T::Timestamp | T::Uuid | T::Decimal) => {
            serde_json::Value::String(raw.to_owned())
        }
        Some(T::Bool | T::Int | T::Float | T::Json) | None => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
        }
    }
}

fn print_result(result: &portcullis_engine::CallResult) {
    match result.kind {
        ActionKind::Read => {
            if result.rows.is_empty() {
                println!("{}", "no rows matched".dimmed());
            } else {
                print_table(&result.rows);
                if result.truncated {
                    println!("{}", "(truncated at this action's row limit)".yellow());
                }
            }
        }
        ActionKind::Write => {
            println!("{} row(s) written", result.rows_affected.to_string().bold());
            if !result.rows.is_empty() {
                print_table(&result.rows);
            }
            if result.replayed {
                println!("{}", "(replay of an earlier identical write)".yellow());
            }
        }
    }
    println!(
        "{}",
        format!(
            "{} in {}ms  request {}",
            result.action,
            result.duration.as_millis(),
            result.request_id
        )
        .dimmed()
    );
}

fn print_table(rows: &portcullis_db::plan::Rows) {
    let mut widths: Vec<usize> = rows.columns.iter().map(String::len).collect();
    let cells: Vec<Vec<String>> = rows
        .rows
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let header: Vec<String> = rows
        .columns
        .iter()
        .zip(&widths)
        .map(|(c, w)| format!("{c:<w$}"))
        .collect();
    println!("{}", header.join("  ").bold());
    for row in &cells {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", line.join("  "));
    }
}

async fn approvals(what: ApprovalCommand) -> Result<ExitCode, Error> {
    match what {
        ApprovalCommand::List { common } => {
            let (engine, _) = build(&common).await?;
            let pending = engine.approvals().pending();
            if common.json {
                println!("{}", serde_json::to_string_pretty(&pending)?);
                return Ok(ExitCode::SUCCESS);
            }
            if pending.is_empty() {
                println!("{}", "nothing is waiting for approval".dimmed());
            }
            for a in pending {
                println!("{}  {}", a.id.bold(), a.action);
                println!("      raised {} by {}", a.created, a.caller.id);
                println!("      {}", a.reason.yellow());
                println!("      {}", a.args);
            }
            Ok(ExitCode::SUCCESS)
        }
        ApprovalCommand::Approve { id, common } => {
            let (engine, _) = build(&common).await?;
            let approver = caller_for(&engine, &common)?;
            let result = engine.approve(&id, &approver).await?;
            println!("{} {}", "approved".green().bold(), id);
            print_result(&result);
            engine.flush_audit()?;
            Ok(ExitCode::SUCCESS)
        }
        ApprovalCommand::Deny { id, common } => {
            let (engine, _) = build(&common).await?;
            let approver = caller_for(&engine, &common)?;
            let a = engine.deny(&id, &approver)?;
            println!("{} {} ({})", "denied".red().bold(), a.id, a.action);
            engine.flush_audit()?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn audit(what: AuditCommand) -> Result<ExitCode, Error> {
    match what {
        AuditCommand::Verify { common } => {
            // Reading the log needs the path, not a database connection.
            let config = Config::load(&common.config)?;
            let report = AuditLog::verify(&config.audit.path)?;
            if common.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "intact": report.is_intact(),
                        "records": report.records,
                        "broken_at": report.broken_at,
                        "head": report.head,
                    })
                );
            } else if report.is_intact() {
                println!(
                    "{} {} record(s), chain intact",
                    "ok".green().bold(),
                    report.records
                );
                println!("head {}", report.head.dimmed());
            } else {
                println!(
                    "{} chain breaks at line {}",
                    "TAMPERED".red().bold(),
                    report.broken_at.unwrap_or_default()
                );
                println!("{} record(s) verified before that point", report.records);
            }
            Ok(if report.is_intact() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        AuditCommand::Tail { lines, common } => {
            let config = Config::load(&common.config)?;
            let text = std::fs::read_to_string(&config.audit.path).unwrap_or_default();
            let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            for line in all.iter().rev().take(lines).rev() {
                if common.json {
                    println!("{line}");
                } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let decision = v["decision"].as_str().unwrap_or("?");
                    let coloured = match decision {
                        "allowed" => decision.green().to_string(),
                        "denied" => decision.red().to_string(),
                        "pending" => decision.yellow().to_string(),
                        _ => decision.to_string(),
                    };
                    println!(
                        "{}  {:<9} {:<18} {:<10} rows={:<4} {}ms",
                        v["ts"].as_str().unwrap_or("?").dimmed(),
                        coloured,
                        v["action"].as_str().unwrap_or("?"),
                        v["caller"].as_str().unwrap_or("?"),
                        v["rows"].as_u64().unwrap_or(0),
                        v["duration_ms"].as_u64().unwrap_or(0),
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn doctor(common: &Common) -> Result<ExitCode, Error> {
    let mut problems = 0;

    println!("{}", "configuration".bold());
    let config = match Config::load(&common.config) {
        Ok(c) => {
            println!("  {} {}", "ok".green(), common.config.display());
            c
        }
        Err(e) => {
            println!("  {} {e}", "fail".red());
            return Ok(ExitCode::from(2));
        }
    };
    if config.server.mask_salt == "change-me-in-production" {
        println!("  {} mask_salt is still the example value", "warn".yellow());
        problems += 1;
    }
    if config.audit.fsync == Fsync::Never {
        println!("  {} audit fsync is \"never\"", "warn".yellow());
        problems += 1;
    }

    println!("{}", "backend".bold());
    let (engine, warnings) = match Engine::build(config).await {
        Ok(v) => v,
        Err(e) => {
            println!("  {} {e}", "fail".red());
            return Ok(ExitCode::from(2));
        }
    };
    println!("  {} {}", "ok".green(), engine.backend_description());
    match engine.health().await {
        Ok(()) => println!("  {} responds to a health check", "ok".green()),
        Err(e) => {
            println!("  {} {e}", "fail".red());
            problems += 1;
        }
    }
    println!(
        "  {} {} table(s) visible",
        "ok".green(),
        engine.schema().tables.len()
    );

    println!("{}", "actions".bold());
    println!("  {} {} published", "ok".green(), engine.registry().len());
    for w in &warnings {
        println!("  {} {}: {}", "warn".yellow(), w.action, w.message);
        problems += 1;
    }

    println!("{}", "audit".bold());
    let report = AuditLog::verify(engine.config().audit.path.as_path())?;
    if report.is_intact() {
        println!(
            "  {} {} record(s), chain intact",
            "ok".green(),
            report.records
        );
    } else {
        println!(
            "  {} chain breaks at line {}",
            "FAIL".red().bold(),
            report.broken_at.unwrap_or_default()
        );
        problems += 1;
    }

    println!();
    if problems == 0 {
        println!("{}", "no problems found".green().bold());
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "{}",
            format!("{problems} thing(s) to look at").yellow().bold()
        );
        Ok(ExitCode::from(1))
    }
}

/// Print an error the way an operator wants to read it.
fn report(e: &Error) {
    eprintln!("{} {e}", "error:".red().bold());
    let bin = branding::BIN;
    let hint = match e {
        Error::Config(_) => Some(format!("run `{bin} doctor` for a fuller check")),
        Error::Validation { .. } => Some(format!(
            "fix the action in the configuration file, then re-run `{bin} validate`"
        )),
        Error::Denied { .. } => {
            Some("check the `allow` list of the role you passed with --role".to_owned())
        }
        Error::ApprovalRequired { .. } => {
            Some(format!("release it with `{bin} approvals approve <id>`"))
        }
        Error::Approval(_) => Some(format!(
            "list what is still waiting with `{bin} approvals list`"
        )),
        Error::Backend(_) => Some(format!(
            "check the database is reachable with `{bin} doctor`"
        )),
        _ => None,
    };
    if let Some(h) = hint {
        eprintln!("{} {h}", "hint:".cyan().bold());
    }
}
