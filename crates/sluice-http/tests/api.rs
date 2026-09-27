//! End-to-end tests over the HTTP surface.
//!
//! These drive the real router with a real engine on the in-memory backend, so
//! they need nothing installed. What they check is the thing stdio could not
//! do: two callers hitting one process get different scopes, decided by their
//! credential rather than by a process-wide flag.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sluice_engine::{Config, Engine};
use sluice_http::HttpConfig;
use sluice_http::auth::{ApiKey, ApiKeyAuthenticator};
use tower::ServiceExt;

const FIXTURE: &str = r#"{
  "tables": [
    {
      "name": "public.orders",
      "primary_key": ["order_no"],
      "columns": [
        { "name": "order_no", "type": "text", "nullable": false },
        { "name": "region", "type": "text", "nullable": false },
        { "name": "total", "type": "decimal", "nullable": false },
        { "name": "customer_email", "type": "text", "nullable": false }
      ],
      "rows": [
        { "order_no": "8812", "region": "EU", "total": "1200.00", "customer_email": "alice@example.com" },
        { "order_no": "8813", "region": "US", "total": "50.00",  "customer_email": "bob@example.com" }
      ]
    },
    {
      "name": "public.refunds",
      "primary_key": ["refund_id"],
      "columns": [
        { "name": "refund_id", "type": "uuid", "nullable": false },
        { "name": "order_no", "type": "text", "nullable": false },
        { "name": "region", "type": "text", "nullable": false },
        { "name": "amount", "type": "decimal", "nullable": false },
        { "name": "issued_by", "type": "text", "nullable": false }
      ],
      "rows": []
    }
  ]
}"#;

const CONFIG: &str = r#"
[server]
name = "http-test"
mask_salt = "pepper"

[backend]
kind = "memory"
fixtures = "data.json"

[audit]
path = "audit.jsonl"
fsync = "never"

[approvals]
approver_roles = ["manager"]

[http]
listen = "127.0.0.1:0"
console = true

[[role]]
name = "support"
allow = ["find_order", "refund_order"]
attributes = { region = "EU" }

[[role]]
name = "manager"
allow = ["find_order", "refund_order"]
attributes = { region = "EU" }

[action.find_order]
description = "Look up an order."
table = "orders"
params = { order_no = { type = "text", required = true } }
returns = ["order_no", "total", "customer_email"]
filter = "order_no = :order_no"
row_filter = "region = $caller.region"
mask = { customer_email = "partial" }
max_rows = 5

[action.refund_order]
description = "Refund an order."
table = "refunds"
params = { order_no = { type = "text", required = true }, amount = { type = "decimal", required = true } }
row_filter = "region = $caller.region"

[action.refund_order.write]
mode = "insert"
columns = { refund_id = "uuid()", order_no = ":order_no", amount = ":amount", issued_by = "$caller.id" }
idempotency = ["order_no", "amount"]
returning = ["refund_id", "region"]

[action.refund_order.approval]
over_param = "amount"
over_amount = "500.00"
"#;

/// Keys the tests present. The role and region come from the key, not a flag.
const EU_KEY: &str = "sk_test_eu";
const US_KEY: &str = "sk_test_us";
const MANAGER_KEY: &str = "sk_test_manager";

struct Harness {
    app: axum::Router,
    dir: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    async fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("sluice-http-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.json"), FIXTURE).unwrap();
        let config = Config::parse(CONFIG, &dir).unwrap();
        let (engine, warnings) = Engine::build(config).await.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");

        let key = |material: &str, role: &str, caller: &str, region: &str| ApiKey {
            hash: ApiKeyAuthenticator::hash(material),
            role: role.to_owned(),
            caller: caller.to_owned(),
            attributes: std::collections::BTreeMap::from([(
                "region".to_string(),
                sluice_core::Value::Text(region.to_owned()),
            )]),
        };
        let auth = Arc::new(ApiKeyAuthenticator::new(vec![
            key(EU_KEY, "support", "alice", "EU"),
            key(US_KEY, "support", "bob", "US"),
            key(MANAGER_KEY, "manager", "jane", "EU"),
        ]));

        let app = sluice_http::router(Arc::new(engine), auth, &HttpConfig::default());
        Self { app, dir }
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    async fn rpc(&self, key: &str, payload: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(payload.to_string()))
            .unwrap();
        self.send(request).await
    }

    async fn call_tool(&self, key: &str, name: &str, arguments: Value) -> Value {
        let (status, body) = self
            .rpc(
                key,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": name, "arguments": arguments }
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["result"].clone()
    }

    async fn get(&self, path: &str, key: Option<&str>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("GET").uri(path);
        if let Some(k) = key {
            builder = builder.header("authorization", format!("Bearer {k}"));
        }
        self.send(builder.body(Body::empty()).unwrap()).await
    }

    async fn post(&self, path: &str, key: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }
}

#[tokio::test]
async fn a_request_with_no_credential_is_refused() {
    let h = Harness::new("anon").await;
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string(),
        ))
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("www-authenticate").unwrap(),
        "Bearer"
    );
}

#[tokio::test]
async fn a_bad_credential_is_refused() {
    let h = Harness::new("badkey").await;
    let (status, body) = h
        .rpc(
            "sk_not_issued",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "credential_rejected");
}

#[tokio::test]
async fn two_callers_on_one_process_get_different_scopes() {
    let h = Harness::new("scopes").await;

    // 8812 is an EU order. The EU key sees it; the US key does not. Same
    // process, same action, different credential.
    let eu = h
        .call_tool(EU_KEY, "find_order", json!({"order_no": "8812"}))
        .await;
    assert_eq!(eu["structuredContent"]["row_count"], 1);
    assert_eq!(
        eu["structuredContent"]["rows"][0]["customer_email"], "a***@example.com",
        "masking still applies"
    );

    let us = h
        .call_tool(US_KEY, "find_order", json!({"order_no": "8812"}))
        .await;
    assert_eq!(us["structuredContent"]["row_count"], 0);

    // And the US key sees its own order.
    let theirs = h
        .call_tool(US_KEY, "find_order", json!({"order_no": "8813"}))
        .await;
    assert_eq!(theirs["structuredContent"]["row_count"], 1);
}

#[tokio::test]
async fn tools_list_describes_the_published_actions() {
    let h = Harness::new("tools").await;
    let (status, body) = h
        .rpc(
            EU_KEY,
            json!({"jsonrpc":"2.0","id":7,"method":"tools/list"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], 7);
    let tools = body["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    let refund = tools.iter().find(|t| t["name"] == "refund_order").unwrap();
    assert_eq!(refund["annotations"]["readOnlyHint"], false);
    assert_eq!(refund["annotations"]["idempotentHint"], true);
    assert!(
        refund["description"].as_str().unwrap().contains("approval"),
        "the gate should be visible to the model"
    );
}

#[tokio::test]
async fn a_notification_is_accepted_with_no_body() {
    let h = Harness::new("notify").await;
    let (status, body) = h
        .rpc(
            EU_KEY,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn batches_and_streams_are_refused_clearly() {
    let h = Harness::new("shape").await;

    let (status, body) = h
        .rpc(EU_KEY, json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "batch_not_supported");

    let (status, body) = h.get("/mcp", Some(EU_KEY)).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"], "sse_not_supported");
}

#[tokio::test]
async fn an_unknown_protocol_version_is_reported() {
    let h = Harness::new("version").await;
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {EU_KEY}"))
        .header("mcp-protocol-version", "1999-01-01")
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}).to_string(),
        ))
        .unwrap();
    let (status, body) = h.send(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "unsupported_protocol_version");
}

#[tokio::test]
async fn the_approval_queue_works_over_http() {
    let h = Harness::new("approvals").await;

    // Over the threshold, so it parks instead of writing.
    let parked = h
        .call_tool(
            EU_KEY,
            "refund_order",
            json!({"order_no": "8812", "amount": "900.00"}),
        )
        .await;
    assert_eq!(parked["isError"], true);
    assert_eq!(parked["structuredContent"]["error"], "approval_required");

    // The requester sees it but cannot decide it.
    let (status, body) = h.get("/api/approvals", Some(EU_KEY)).await;
    assert_eq!(status, StatusCode::OK);
    let pending = body["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["decidable"], false, "support may not approve");
    let id = pending[0]["id"].as_str().unwrap().to_owned();

    let (status, body) = h
        .post(&format!("/api/approvals/{id}/approve"), EU_KEY)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A manager can, and the write happens then.
    let (status, body) = h.get("/api/approvals", Some(MANAGER_KEY)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pending"][0]["decidable"], true);

    let (status, body) = h
        .post(&format!("/api/approvals/{id}/approve"), MANAGER_KEY)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["rows_affected"], 1);
    // The row filter put the caller's region on the row.
    assert_eq!(body["result"]["returned"][0]["region"], "EU");

    // Releasing twice is a conflict, not a second write.
    let (status, _) = h
        .post(&format!("/api/approvals/{id}/approve"), MANAGER_KEY)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_unknown_approval_is_a_404() {
    let h = Harness::new("missing-approval").await;
    let (status, _) = h.post("/api/approvals/apr_nope/approve", MANAGER_KEY).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn health_readiness_and_metrics_need_no_credential() {
    let h = Harness::new("ops").await;

    let (status, body) = h.get("/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (status, body) = h.get("/readyz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");

    // Drive one call so the counters have something in them.
    let _ = h
        .call_tool(EU_KEY, "find_order", json!({"order_no": "8812"}))
        .await;

    let request = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        text.contains("sluice_tool_calls_total{action=\"find_order\",outcome=\"ok\"} 1"),
        "{text}"
    );
    assert!(text.contains("sluice_http_requests_total"), "{text}");
}

#[tokio::test]
async fn the_console_is_served_and_carries_no_secrets() {
    let h = Harness::new("console").await;
    let request = Request::builder().uri("/").body(Body::empty()).unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("<title>Sluice approvals</title>"));
    assert!(
        !html.contains(EU_KEY),
        "the page must not embed a credential"
    );
}

#[tokio::test]
async fn a_refused_tool_call_is_a_result_not_a_transport_error() {
    let h = Harness::new("toolerror").await;
    // Missing a required argument.
    let result = h.call_tool(EU_KEY, "find_order", json!({})).await;
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["error"], "bad_argument");
}
