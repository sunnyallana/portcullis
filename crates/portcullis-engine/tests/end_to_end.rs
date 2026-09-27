//! End-to-end tests over the whole request path.
//!
//! Each test builds a throwaway deployment on the in-memory backend, so the
//! suite runs anywhere with no database, and exercises the same `Engine::call`
//! that the MCP server uses. What is asserted here is the product's promise:
//! an agent cannot reach outside its scope, cannot write SQL, cannot write
//! twice by retrying, and cannot do anything that is not in the log.

use std::path::PathBuf;

use portcullis_core::{AuditLog, Error};
use portcullis_engine::{Config, Engine};

const FIXTURE: &str = r#"{
  "tables": [
    {
      "name": "public.orders",
      "primary_key": ["order_no"],
      "columns": [
        { "name": "order_no", "type": "text", "nullable": false },
        { "name": "region", "type": "text", "nullable": false },
        { "name": "status", "type": "text", "nullable": false },
        { "name": "total", "type": "decimal", "nullable": false },
        { "name": "customer_email", "type": "text", "nullable": false }
      ],
      "rows": [
        { "order_no": "8812", "region": "EU", "status": "open", "total": "1200.00", "customer_email": "alice@example.com" },
        { "order_no": "8813", "region": "US", "status": "open", "total": "50.00",  "customer_email": "bob@example.com" }
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
name = "test"
mask_salt = "pepper"

[backend]
kind = "memory"
fixtures = "data.json"

[audit]
path = "audit.jsonl"
fsync = "never"

[approvals]
approver_roles = ["manager"]

[[role]]
name = "support_eu"
allow = ["find_order", "refund_order"]
attributes = { region = "EU" }

[[role]]
name = "manager"
allow = ["find_order", "refund_order"]
attributes = { region = "EU" }

[[role]]
name = "readonly"
allow = ["find_order"]
attributes = { region = "EU" }

[action.find_order]
description = "Look up an order."
table = "orders"
params = { order_no = { type = "text", required = true }, status = { type = "text", required = false, one_of = ["open", "held"] } }
returns = ["order_no", "status", "total", "customer_email"]
filter = "order_no = :order_no and status = :status"
row_filter = "region = $caller.region"
mask = { customer_email = "partial" }
max_rows = 10
rate_limit = 3

[action.refund_order]
description = "Refund an order."
table = "refunds"
params = { order_no = { type = "text", required = true }, amount = { type = "decimal", required = true } }
row_filter = "region = $caller.region"

[action.refund_order.write]
mode = "insert"
columns = { refund_id = "uuid()", order_no = ":order_no", amount = ":amount", issued_by = "$caller.id" }
idempotency = ["order_no", "amount"]
returning = ["refund_id", "order_no", "amount", "region"]

[action.refund_order.approval]
over_param = "amount"
over_amount = "500.00"
"#;

struct Deployment {
    engine: Engine,
    dir: PathBuf,
}

impl Deployment {
    async fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("portcullis-e2e-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.json"), FIXTURE).unwrap();
        let config = Config::parse(CONFIG, &dir).unwrap();
        let (engine, warnings) = Engine::build(config).await.unwrap();
        assert!(
            warnings.is_empty(),
            "the test configuration should validate cleanly: {warnings:?}"
        );
        Self { engine, dir }
    }

    fn audit_path(&self) -> PathBuf {
        self.dir.join("audit.jsonl")
    }

    fn audit_lines(&self) -> Vec<serde_json::Value> {
        self.engine.flush_audit().unwrap();
        std::fs::read_to_string(self.audit_path())
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Deployment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn a_caller_cannot_see_rows_outside_their_scope() {
    let d = Deployment::new("scope").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();

    let mine = d
        .engine
        .call("find_order", &serde_json::json!({"order_no": "8812"}), &eu)
        .await
        .unwrap();
    assert_eq!(mine.rows.len(), 1);

    // 8813 exists, belongs to US, and is invisible rather than forbidden:
    // the agent learns nothing about rows it may not see.
    let theirs = d
        .engine
        .call("find_order", &serde_json::json!({"order_no": "8813"}), &eu)
        .await
        .unwrap();
    assert_eq!(theirs.rows.len(), 0);
}

#[tokio::test]
async fn an_argument_full_of_sql_is_just_data() {
    let d = Deployment::new("injection").await;
    // A caller each, so this test measures injection and not the rate limit.
    for (i, attempt) in [
        "8812' OR '1'='1",
        "8812; DROP TABLE orders",
        "8812' UNION SELECT * FROM refunds --",
        "' OR region <> 'EU",
    ]
    .into_iter()
    .enumerate()
    {
        let caller = d
            .engine
            .caller("support_eu", &format!("probe-{i}"))
            .unwrap();
        let result = d
            .engine
            .call(
                "find_order",
                &serde_json::json!({ "order_no": attempt }),
                &caller,
            )
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 0, "`{attempt}` should match nothing");
    }
    // And the table is still there.
    let eu = d.engine.caller("support_eu", "alice").unwrap();
    let ok = d
        .engine
        .call("find_order", &serde_json::json!({"order_no": "8812"}), &eu)
        .await
        .unwrap();
    assert_eq!(ok.rows.len(), 1);
}

#[tokio::test]
async fn masks_apply_to_results_and_to_the_log() {
    let d = Deployment::new("mask").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();
    let result = d
        .engine
        .call("find_order", &serde_json::json!({"order_no": "8812"}), &eu)
        .await
        .unwrap();
    let json = result.rows.to_json();
    assert_eq!(json[0]["customer_email"], "a***@example.com");

    // The same column, arriving as an argument, is masked in the audit too.
    let _ = d
        .engine
        .call(
            "find_order",
            &serde_json::json!({"order_no": "8812", "customer_email": "alice@example.com"}),
            &eu,
        )
        .await;
    let text = std::fs::read_to_string(d.audit_path()).unwrap_or_default();
    d.engine.flush_audit().unwrap();
    let text = if text.is_empty() {
        std::fs::read_to_string(d.audit_path()).unwrap_or_default()
    } else {
        text
    };
    assert!(
        !text.contains("alice@example.com"),
        "the audit log should not hold an unmasked address"
    );
}

#[tokio::test]
async fn a_role_without_the_action_is_refused() {
    let d = Deployment::new("role").await;
    let ro = d.engine.caller("readonly", "auditor-1").unwrap();
    let err = d
        .engine
        .call(
            "refund_order",
            &serde_json::json!({"order_no": "8812", "amount": "10.00"}),
            &ro,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Denied { .. }), "{err}");
}

#[tokio::test]
async fn a_misspelled_action_gets_a_suggestion() {
    let d = Deployment::new("suggest").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();
    let err = d
        .engine
        .call("find_orders", &serde_json::json!({"order_no": "1"}), &eu)
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("did you mean `find_order`"),
        "{err}"
    );
}

#[tokio::test]
async fn arguments_are_checked_before_anything_runs() {
    let d = Deployment::new("args").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();

    let missing = d
        .engine
        .call("find_order", &serde_json::json!({}), &eu)
        .await
        .unwrap_err();
    assert!(format!("{missing}").contains("required"), "{missing}");

    let unknown = d
        .engine
        .call(
            "find_order",
            &serde_json::json!({"order_no": "8812", "limit": 500}),
            &eu,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{unknown}").contains("not a parameter"),
        "{unknown}"
    );

    let bad_enum = d
        .engine
        .call(
            "find_order",
            &serde_json::json!({"order_no": "8812", "status": "cancelled"}),
            &eu,
        )
        .await
        .unwrap_err();
    assert!(
        format!("{bad_enum}").contains("must be one of"),
        "{bad_enum}"
    );
}

#[tokio::test]
async fn a_large_write_is_parked_then_released_exactly_once() {
    let d = Deployment::new("approval").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();

    let err = d
        .engine
        .call(
            "refund_order",
            &serde_json::json!({"order_no": "8812", "amount": "1200.00"}),
            &eu,
        )
        .await
        .unwrap_err();
    let Error::ApprovalRequired { request, .. } = err else {
        panic!("expected an approval to be required, got {err}");
    };

    // Nothing was written while it waited.
    assert_eq!(d.engine.approvals().pending().len(), 1);

    // A role outside `approver_roles` cannot release it.
    let wrong_role = d.engine.caller("readonly", "auditor-1").unwrap();
    let refused = d.engine.approve(&request, &wrong_role).await.unwrap_err();
    assert!(matches!(refused, Error::Denied { .. }), "{refused}");

    let manager = d.engine.caller("manager", "manager-jane").unwrap();
    let released = d.engine.approve(&request, &manager).await.unwrap();
    assert_eq!(released.rows_affected, 1);
    // The row filter was written into the row, not merely checked.
    assert_eq!(released.rows.to_json()[0]["region"], "EU");

    let again = d.engine.approve(&request, &manager).await.unwrap_err();
    assert!(format!("{again}").contains("already executed"), "{again}");
}

#[tokio::test]
async fn a_caller_can_find_out_what_happened_to_its_parked_write() {
    let d = Deployment::new("status").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();

    let err = d
        .engine
        .call(
            "refund_order",
            &serde_json::json!({"order_no": "8812", "amount": "2500.00"}),
            &eu,
        )
        .await
        .unwrap_err();
    let Error::ApprovalRequired { request, .. } = err else {
        panic!("expected an approval to be required, got {err}");
    };

    // The requester can see it is waiting.
    let status = |who: &portcullis_core::Caller| {
        let engine = &d.engine;
        let args = serde_json::json!({ "request": request });
        let who = who.clone();
        async move { engine.call("approval_status", &args, &who).await }
    };

    let waiting = status(&eu).await.unwrap();
    assert_eq!(waiting.rows.to_json()[0]["status"], "pending");
    assert_eq!(waiting.rows.to_json()[0]["action"], "refund_order");
    assert!(waiting.rows.to_json()[0].get("result").is_none());

    // Someone unrelated is told it does not exist rather than that it is
    // theirs to see: a request id must not be a way to probe for other
    // people's calls.
    let stranger = d.engine.caller("readonly", "nosey").unwrap();
    let refused = status(&stranger).await.unwrap_err();
    assert!(
        format!("{refused}").contains("no approval request"),
        "{refused}"
    );

    // Once released, the requester can collect the outcome it was never sent.
    let manager = d.engine.caller("manager", "manager-jane").unwrap();
    d.engine.approve(&request, &manager).await.unwrap();

    let done = status(&eu).await.unwrap();
    let row = done.rows.to_json();
    assert_eq!(row[0]["status"], "executed");
    assert_eq!(row[0]["decided_by"], "manager-jane");
    assert_eq!(row[0]["result"]["rows_affected"], 1);
}

#[tokio::test]
async fn the_status_action_name_cannot_be_taken_by_a_configuration() {
    let dir =
        std::env::temp_dir().join(format!("portcullis-e2e-reserved-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.json"), FIXTURE).unwrap();
    // Rename the action and the grants that reference it, so the only thing
    // wrong with this configuration is the reserved name.
    let clashing = CONFIG
        .replace("[action.find_order]", "[action.approval_status]")
        .replace("\"find_order\"", "\"approval_status\"");
    let config = Config::parse(&clashing, &dir).unwrap();
    let err = Engine::build(config).await.unwrap_err();
    assert!(format!("{err}").contains("reserved"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_approver_cannot_release_their_own_request() {
    let d = Deployment::new("self-approval").await;
    // The manager role may both call the action and approve it, which is the
    // case where self-approval would otherwise slip through.
    let manager = d.engine.caller("manager", "manager-jane").unwrap();

    let err = d
        .engine
        .call(
            "refund_order",
            &serde_json::json!({"order_no": "8812", "amount": "5000.00"}),
            &manager,
        )
        .await
        .unwrap_err();
    let Error::ApprovalRequired { request, .. } = err else {
        panic!("expected an approval to be required, got {err}");
    };

    let own = d.engine.approve(&request, &manager).await.unwrap_err();
    assert!(format!("{own}").contains("may not decide it"), "{own}");

    // Someone else with the same role can.
    let other = d.engine.caller("manager", "manager-sam").unwrap();
    let released = d.engine.approve(&request, &other).await.unwrap();
    assert_eq!(released.rows_affected, 1);
}

#[tokio::test]
async fn a_small_write_goes_straight_through_and_retries_do_not_repeat_it() {
    let d = Deployment::new("idempotency").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();
    let args = serde_json::json!({"order_no": "8812", "amount": "25.00"});

    let first = d.engine.call("refund_order", &args, &eu).await.unwrap();
    assert_eq!(first.rows_affected, 1);
    assert!(!first.replayed);
    let refund_id = first.rows.to_json()[0]["refund_id"].clone();

    let second = d.engine.call("refund_order", &args, &eu).await.unwrap();
    assert!(second.replayed, "a retry must not write a second refund");
    assert_eq!(second.rows.to_json()[0]["refund_id"], refund_id);

    // A different caller issuing the same refund is a different refund.
    let other = d.engine.caller("support_eu", "bob").unwrap();
    let third = d.engine.call("refund_order", &args, &other).await.unwrap();
    assert!(!third.replayed);
}

#[tokio::test]
async fn the_rate_limit_stops_a_runaway_loop() {
    let d = Deployment::new("ratelimit").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();
    let args = serde_json::json!({"order_no": "8812"});
    for _ in 0..3 {
        d.engine.call("find_order", &args, &eu).await.unwrap();
    }
    let err = d.engine.call("find_order", &args, &eu).await.unwrap_err();
    assert!(matches!(err, Error::LimitExceeded(_)), "{err}");
}

#[tokio::test]
async fn every_decision_is_recorded_and_the_chain_verifies() {
    let d = Deployment::new("audit").await;
    let eu = d.engine.caller("support_eu", "alice").unwrap();
    let ro = d.engine.caller("readonly", "auditor-1").unwrap();

    d.engine
        .call("find_order", &serde_json::json!({"order_no": "8812"}), &eu)
        .await
        .unwrap();
    let _ = d
        .engine
        .call(
            "refund_order",
            &serde_json::json!({"order_no": "8812", "amount": "10.00"}),
            &ro,
        )
        .await;
    let _ = d
        .engine
        .call(
            "refund_order",
            &serde_json::json!({"order_no": "8812", "amount": "9000.00"}),
            &eu,
        )
        .await;

    let lines = d.audit_lines();
    assert_eq!(lines.len(), 3, "an allowed, a denied and a pending call");
    assert_eq!(lines[0]["decision"], "allowed");
    assert_eq!(lines[1]["decision"], "denied");
    assert_eq!(lines[2]["decision"], "pending");
    assert!(lines[2]["approval"].is_string());

    let report = AuditLog::verify(d.audit_path()).unwrap();
    assert!(report.is_intact());
    assert_eq!(report.records, 3);
}

#[tokio::test]
async fn an_action_naming_a_missing_column_fails_at_startup() {
    let dir = std::env::temp_dir().join(format!("portcullis-e2e-bad-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.json"), FIXTURE).unwrap();
    let broken = CONFIG.replace(
        r#"returns = ["order_no", "status", "total", "customer_email"]"#,
        r#"returns = ["order_no", "stauts"]"#,
    );
    let config = Config::parse(&broken, &dir).unwrap();
    let err = Engine::build(config).await.unwrap_err();
    let text = format!("{err}");
    assert!(text.contains("stauts"), "{text}");
    assert!(text.contains("did you mean `status`"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
