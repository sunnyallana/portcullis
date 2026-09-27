//! An MCP server over stdio.
//!
//! Newline-delimited JSON-RPC 2.0 on stdin and stdout, with logs on stderr.
//! Requests are handled concurrently and responses are serialised through a
//! single writer task, so a slow query does not block the next tool call and
//! two responses can never interleave on the wire.
//!
//! Protocol errors (malformed JSON, unknown method) become JSON-RPC errors.
//! Failures inside a tool become a normal result with `isError` set, which is
//! what the specification asks for: the model is supposed to see and react to
//! "that order does not exist", not have the transport fail.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sluice_core::{ActionKind, Caller, Error};
use sluice_engine::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Protocol revisions this server understands, newest first.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Server version reported during initialisation.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Json>,
    method: String,
    #[serde(default)]
    params: Json,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Json,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Json>,
}

impl Response {
    fn ok(id: Json, result: Json) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Json, code: i32, message: impl Into<String>, data: Option<Json>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

/// Serve MCP on stdin and stdout until the client closes the stream.
///
/// Returns once stdin reaches end of file and every in-flight call has
/// finished and been written.
pub async fn serve_stdio(engine: Arc<Engine>, caller: Caller) -> sluice_core::Result<()> {
    let (tx, mut rx) = mpsc::channel::<String>(64);

    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err() || out.write_all(b"\n").await.is_err()
            {
                break;
            }
            let _ = out.flush().await;
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = tokio::task::JoinSet::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let engine = Arc::clone(&engine);
        let caller = caller.clone();
        let tx = tx.clone();
        tasks.spawn(async move {
            if let Some(response) = handle_line(&engine, &caller, &line).await {
                let encoded = serde_json::to_string(&response)
                    .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32603,"message":"could not encode response: {e}"}}}}"#));
                let _ = tx.send(encoded).await;
            }
        });
    }

    drop(tx);
    while tasks.join_next().await.is_some() {}
    let _ = writer.await;
    engine.flush_audit()?;
    Ok(())
}

/// Handle one line. `None` means the message was a notification.
async fn handle_line(engine: &Engine, caller: &Caller, line: &str) -> Option<Response> {
    // Some clients (and plenty of shells on Windows) prefix the first line
    // with a byte-order mark. Dropping it costs nothing and saves an hour of
    // someone's afternoon.
    let line = line.trim_start_matches('\u{feff}');

    let request: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return Some(Response::err(
                Json::Null,
                -32700,
                format!("could not parse the request: {e}"),
                None,
            ));
        }
    };

    if request.jsonrpc != "2.0" {
        return Some(Response::err(
            request.id.unwrap_or(Json::Null),
            -32600,
            "only JSON-RPC 2.0 is supported",
            None,
        ));
    }

    // A message with no id is a notification: act on it, answer nothing.
    let Some(id) = request.id.clone() else {
        tracing::debug!(method = %request.method, "notification");
        return None;
    };

    Some(match request.method.as_str() {
        "initialize" => Response::ok(id, initialize(engine, &request.params)),
        "ping" => Response::ok(id, json!({})),
        "tools/list" => Response::ok(id, tools_list(engine)),
        "tools/call" => match tools_call(engine, caller, &request.params).await {
            Ok(result) => Response::ok(id, result),
            Err((code, message)) => Response::err(id, code, message, None),
        },
        other => Response::err(
            id,
            -32601,
            format!("`{other}` is not a method this server implements"),
            Some(json!({ "supported": ["initialize", "ping", "tools/list", "tools/call"] })),
        ),
    })
}

fn initialize(engine: &Engine, params: &Json) -> Json {
    // Echo the client's revision when we speak it; otherwise offer our newest.
    let requested = params.get("protocolVersion").and_then(Json::as_str);
    let version = requested
        .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
        .unwrap_or(SUPPORTED_PROTOCOLS[0]);

    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": engine.config().server.name,
            "version": VERSION,
        },
        "instructions": format!(
            "This server exposes {} governed data actions. Each action is a fixed, \
             permissioned operation; there is no free-form SQL. Arguments are \
             validated and every call is audited.",
            engine.registry().len()
        ),
    })
}

fn tools_list(engine: &Engine) -> Json {
    let tools: Vec<Json> = engine
        .registry()
        .iter()
        .map(|(name, action)| {
            let (read_only, destructive) = action.spec.kind.hints();
            let mut description = action.spec.description.clone();
            if let Some(rule) = &action.spec.approval {
                description.push_str(if rule.always {
                    " Requires human approval before it takes effect."
                } else {
                    " Large values require human approval before they take effect."
                });
            }
            json!({
                "name": name,
                "description": description,
                "inputSchema": action.input_schema,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": destructive,
                    "idempotentHint": action
                        .spec
                        .write
                        .as_ref()
                        .is_some_and(|w| !w.idempotency.is_empty()),
                },
            })
        })
        .collect();
    json!({ "tools": tools })
}

async fn tools_call(
    engine: &Engine,
    caller: &Caller,
    params: &Json,
) -> Result<Json, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Json::as_str)
        .ok_or((-32602, "`name` is required".to_owned()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match engine.call(name, &arguments, caller).await {
        Ok(result) => {
            let structured = result.to_json();
            Ok(json!({
                "content": [{ "type": "text", "text": render(result.kind, &structured) }],
                "structuredContent": structured,
                "isError": false,
            }))
        }
        // A refusal is information the model should act on, not a transport
        // failure, so it comes back as a tool result.
        Err(e) => Ok(json!({
            "content": [{ "type": "text", "text": explain(&e) }],
            "structuredContent": { "error": e.code(), "message": e.to_string() },
            "isError": true,
        })),
    }
}

/// Render a result as text for clients that do not read structured content.
fn render(kind: ActionKind, structured: &Json) -> String {
    match kind {
        ActionKind::Read => {
            let count = structured
                .get("row_count")
                .and_then(Json::as_u64)
                .unwrap_or(0);
            let rows = structured.get("rows").cloned().unwrap_or(Json::Null);
            let body = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| rows.to_string());
            if count == 0 {
                "No rows matched.".to_owned()
            } else {
                format!("{count} row(s):\n{body}")
            }
        }
        ActionKind::Write => {
            let affected = structured
                .get("rows_affected")
                .and_then(Json::as_u64)
                .unwrap_or(0);
            let mut text = format!("{affected} row(s) written.");
            if let Some(returned) = structured.get("returned") {
                text.push('\n');
                text.push_str(
                    &serde_json::to_string_pretty(returned)
                        .unwrap_or_else(|_| returned.to_string()),
                );
            }
            if structured.get("replayed").and_then(Json::as_bool) == Some(true) {
                text.push_str("\n(This write had already happened; the original result is shown.)");
            }
            text
        }
    }
}

/// Phrase an error so the model can do something useful with it.
fn explain(e: &Error) -> String {
    match e {
        Error::ApprovalRequired { action, request } => format!(
            "`{action}` has been sent for human approval as request {request}. \
             Nothing has changed yet. Tell the user it is awaiting approval; do not retry."
        ),
        Error::Denied { .. } => format!("{e}. Do not retry; this is a permission boundary."),
        Error::LimitExceeded(_) => format!("{e}. Wait before trying again."),
        Error::BadArgument { .. } | Error::UnknownAction(_) => format!("{e}"),
        Error::Backend(_) | Error::Io(_) | Error::Json(_) => {
            format!("The request could not be completed: {e}")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_protocol_revision_is_echoed_back() {
        let params = json!({ "protocolVersion": "2024-11-05" });
        let requested = params.get("protocolVersion").and_then(Json::as_str);
        let chosen = requested
            .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
            .unwrap_or(SUPPORTED_PROTOCOLS[0]);
        assert_eq!(chosen, "2024-11-05");
    }

    #[test]
    fn an_unknown_protocol_revision_falls_back_to_ours() {
        let params = json!({ "protocolVersion": "1999-01-01" });
        let requested = params.get("protocolVersion").and_then(Json::as_str);
        let chosen = requested
            .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
            .unwrap_or(SUPPORTED_PROTOCOLS[0]);
        assert_eq!(chosen, SUPPORTED_PROTOCOLS[0]);
    }

    #[test]
    fn an_approval_is_explained_as_pending_not_as_a_failure() {
        let text = explain(&Error::ApprovalRequired {
            action: "refund_order".into(),
            request: "apr_123".into(),
        });
        assert!(text.contains("apr_123"));
        assert!(text.contains("Nothing has changed"));
        assert!(text.contains("do not retry"));
    }

    #[test]
    fn empty_reads_say_so_plainly() {
        let text = render(ActionKind::Read, &json!({ "row_count": 0, "rows": [] }));
        assert_eq!(text, "No rows matched.");
    }
}
