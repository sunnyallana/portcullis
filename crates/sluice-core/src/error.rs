use thiserror::Error;

/// Result alias used throughout Sluice.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure a Sluice request can produce.
///
/// Variants are split by *who* is at fault, because the MCP layer maps them to
/// different JSON-RPC codes and the audit log records them differently: an
/// operator config mistake is not the same event as an agent passing a bad
/// argument, which is not the same as the database being down.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration file could not be read or understood.
    #[error("configuration is invalid: {0}")]
    Config(String),

    /// An action definition does not line up with the live database schema.
    #[error("action `{action}` is invalid: {problem}")]
    Validation {
        /// Action the problem was found in.
        action: String,
        /// What is wrong, phrased for the operator who wrote the file.
        problem: String,
    },

    /// A filter or column expression failed to parse.
    #[error("expression error at character {at}: {problem}")]
    Parse {
        /// Byte offset into the expression.
        at: usize,
        /// What the parser expected.
        problem: String,
    },

    /// The caller passed an argument the action will not accept.
    #[error("argument `{param}` is not acceptable: {problem}")]
    BadArgument {
        /// Parameter name as declared in the action.
        param: String,
        /// Why it was rejected.
        problem: String,
    },

    /// No action by that name is published.
    #[error("no action named `{0}` is published")]
    UnknownAction(String),

    /// The caller's role does not grant this action.
    #[error("role `{role}` may not call `{action}`")]
    Denied {
        /// Caller's role.
        role: String,
        /// Action attempted.
        action: String,
    },

    /// A row filter needs a caller attribute the identity layer did not supply.
    ///
    /// This fails the call closed: a scope that silently disappears is the one
    /// failure this system exists to prevent.
    #[error("the caller has no `{attribute}` attribute, which this action requires")]
    MissingCallerAttribute {
        /// Attribute the filter referenced.
        attribute: String,
    },

    /// The call was accepted but parked for human approval.
    #[error("`{action}` needs approval; request {request} is pending")]
    ApprovalRequired {
        /// Action attempted.
        action: String,
        /// Identifier the approver quotes to release or deny it.
        request: String,
    },

    /// An approval request could not be acted on.
    #[error("{0}")]
    Approval(String),

    /// The action exceeded a configured limit.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    /// The database refused or failed the statement.
    #[error("database error: {0}")]
    Backend(String),

    /// Local I/O failed (audit log, approvals store, config read).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encode/decode failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// A short, stable machine code for logs, metrics and audit records.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config_invalid",
            Self::Validation { .. } => "action_invalid",
            Self::Parse { .. } => "expression_invalid",
            Self::BadArgument { .. } => "bad_argument",
            Self::UnknownAction(_) => "unknown_action",
            Self::Denied { .. } => "denied",
            Self::MissingCallerAttribute { .. } => "caller_attribute_missing",
            Self::ApprovalRequired { .. } => "approval_required",
            Self::Approval(_) => "approval_invalid",
            Self::LimitExceeded(_) => "limit_exceeded",
            Self::Backend(_) => "backend_error",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_error",
        }
    }

    /// True when the caller could fix this by changing their request.
    pub fn is_caller_fault(&self) -> bool {
        matches!(
            self,
            Self::BadArgument { .. }
                | Self::UnknownAction(_)
                | Self::Denied { .. }
                | Self::LimitExceeded(_)
        )
    }
}
