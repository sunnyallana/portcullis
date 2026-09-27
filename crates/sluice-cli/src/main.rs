//! The `sluice` command line.
//!
//! Every subcommand loads the same configuration file and, apart from `init`,
//! connects to the same backend, so what `validate` checks is what `serve`
//! publishes. Logs always go to stderr: `serve` owns stdout for the protocol.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use owo_colors::OwoColorize;
use sluice_core::audit::Fsync;
use sluice_core::{ActionKind, AuditLog, Caller, Error};
use sluice_engine::{Config, Engine};

const DEFAULT_CONFIG: &str = "sluice.toml";
const EXAMPLE_CONFIG: &str = include_str!("../../../examples/orders.toml");
const EXAMPLE_FIXTURE: &str = include_str!("../../../examples/demo-data.json");

/// A governed data-action layer for AI agents.
#[derive(Debug, Parser)]
#[command(name = "sluice", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args, Clone)]
struct Common {
    /// Path to the configuration file.
    #[arg(short, long, env = "SLUICE_CONFIG", default_value = DEFAULT_CONFIG, global = true)]
    config: PathBuf,

    /// Role to act as. Defaults to the only role, when there is only one.
    #[arg(long, env = "SLUICE_ROLE", global = true)]
    role: Option<String>,

    /// Caller identity recorded in the audit log.
    #[arg(long, env = "SLUICE_CALLER", default_value = "cli", global = true)]
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

    /// Serve MCP over stdin and stdout.
    Serve {
        #[command(flatten)]
        common: Common,
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
    let filter = EnvFilter::try_from_env("SLUICE_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
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
        Command::Serve { common } => serve(&common).await,
        Command::Call {
            action,
            args,
            common,
        } => call(&common, &action, &args).await,
        Command::Approvals { what } => approvals(what).await,
        Command::Audit { what } => audit(what),
        Command::Doctor { common } => doctor(&common).await,
    }
}

fn init(directory: &PathBuf, force: bool) -> Result<ExitCode, Error> {
    std::fs::create_dir_all(directory)?;
    let config = directory.join(DEFAULT_CONFIG);
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
    println!("  sluice validate --config {}", config.display());
    println!(
        "  sluice call --config {} --action find_order --arg order_no=8812 --role support_eu",
        config.display()
    );
    println!("  sluice serve --config {}", config.display());
    Ok(ExitCode::SUCCESS)
}

async fn build(common: &Common) -> Result<(Engine, Vec<sluice_engine::Warning>), Error> {
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

async fn serve(common: &Common) -> Result<ExitCode, Error> {
    let (engine, warnings) = build(common).await?;
    for w in &warnings {
        tracing::warn!(action = %w.action, "{}", w.message);
    }
    let caller = caller_for(&engine, common)?;
    tracing::info!(
        backend = %engine.backend_description(),
        actions = engine.registry().len(),
        role = %caller.role,
        "serving MCP on stdio"
    );
    let engine = Arc::new(engine);
    sluice_mcp::serve_stdio(Arc::clone(&engine), caller).await?;
    engine.flush_audit()?;
    Ok(ExitCode::SUCCESS)
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
fn shell_value(declared: Option<sluice_core::DataType>, raw: &str) -> serde_json::Value {
    use sluice_core::DataType as T;
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

fn print_result(result: &sluice_engine::CallResult) {
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

fn print_table(rows: &sluice_db::plan::Rows) {
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
            let result = engine.approve(&id, &common.caller).await?;
            println!("{} {}", "approved".green().bold(), id);
            print_result(&result);
            engine.flush_audit()?;
            Ok(ExitCode::SUCCESS)
        }
        ApprovalCommand::Deny { id, common } => {
            let (engine, _) = build(&common).await?;
            let a = engine.deny(&id, &common.caller)?;
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
    let hint = match e {
        Error::Config(_) => Some("run `sluice doctor` for a fuller check"),
        Error::Validation { .. } => {
            Some("fix the action in the configuration file, then re-run `sluice validate`")
        }
        Error::Denied { .. } => Some("check the `allow` list of the role you passed with --role"),
        Error::ApprovalRequired { .. } => Some("release it with `sluice approvals approve <id>`"),
        Error::Approval(_) => Some("list what is still waiting with `sluice approvals list`"),
        Error::Backend(_) => Some("check the database is reachable with `sluice doctor`"),
        _ => None,
    };
    if let Some(h) = hint {
        eprintln!("{} {h}", "hint:".cyan().bold());
    }
}
