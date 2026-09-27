//! The product's name, in the few places code has to know it.
//!
//! These are separate constants that happen to agree today, not one value
//! derived from another. That is deliberate.
//!
//! Three of them stop being a name the moment the product ships and become a
//! **public interface**: environment variables live in other people's systemd
//! units and Helm charts, and metric series live in their dashboards and alert
//! rules. If a rebrand moved those automatically, every dashboard would go
//! blank and every deployment would stop reading its configuration, and
//! neither would raise an error. They are marked frozen below; changing one is
//! a breaking change that needs a deprecation window, not a find-and-replace.
//!
//! Crate names are not here because they cannot be: `portcullis_core` is a
//! compile-time identifier, and no constant can rename it. A rebrand renames
//! the crates mechanically.

/// The product name, as written in prose, page titles and generated headers.
///
/// Safe to change at any time.
pub const NAME: &str = "Portcullis";

/// The binary a user types.
///
/// Changing it breaks other people's scripts, so treat it as an interface
/// even though it is not frozen the way the two below are.
pub const BIN: &str = "portcullis";

/// Prefix on every environment variable this program reads.
///
/// **Frozen.** See [`env`].
pub const ENV_PREFIX: &str = "PORTCULLIS_";

/// Prefix on every metric series this program exposes.
///
/// **Frozen.** See [`metrics`].
pub const METRIC_PREFIX: &str = "portcullis_";

/// Default configuration file name.
pub const CONFIG_FILE: &str = "portcullis.toml";

/// Default audit log name. Operators override it in `[audit] path`.
pub const AUDIT_FILE: &str = "portcullis-audit.jsonl";

/// Default approvals store name. Operators override it in `[approvals] path`.
pub const APPROVALS_FILE: &str = "portcullis-approvals.jsonl";

/// Default OIDC claim carrying the caller's role.
///
/// A deployment overrides it in `[auth] role_claim`; this is only the name
/// used when it says nothing.
pub const DEFAULT_ROLE_CLAIM: &str = "portcullis_role";

/// Identity the profiler records for its own sampling reads.
pub const PROFILE_CALLER: &str = "portcullis-profile";

/// Environment variables, in one list so the consistency test can see them.
///
/// **Frozen at 1.0.** Adding a variable is fine. Renaming one is a breaking
/// change: read the old name as well for at least one release, and say so in
/// the changelog.
pub mod env {
    /// Path to the configuration file.
    pub const CONFIG: &str = "PORTCULLIS_CONFIG";
    /// Role the CLI acts as.
    pub const ROLE: &str = "PORTCULLIS_ROLE";
    /// Caller identity recorded in the audit log.
    pub const CALLER: &str = "PORTCULLIS_CALLER";
    /// Log filter.
    pub const LOG: &str = "PORTCULLIS_LOG";

    /// Every variable the program reads, for the consistency test.
    pub const ALL: &[&str] = &[CONFIG, ROLE, CALLER, LOG];
}

/// Metric series names, in one list so the consistency test can see them.
///
/// **Frozen at 1.0.** These are in other people's dashboards.
pub mod metrics {
    /// HTTP responses by route and status.
    pub const HTTP_REQUESTS: &str = "portcullis_http_requests_total";
    /// Tool calls by action and outcome.
    pub const TOOL_CALLS: &str = "portcullis_tool_calls_total";
    /// Rejected credentials by reason.
    pub const AUTH_FAILURES: &str = "portcullis_auth_failures_total";
    /// Tool call wall time.
    pub const TOOL_DURATION: &str = "portcullis_tool_call_duration_seconds";

    /// Every series this program exposes, for the consistency test.
    pub const ALL: &[&str] = &[HTTP_REQUESTS, TOOL_CALLS, AUTH_FAILURES, TOOL_DURATION];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enforcement mechanism. Threading a constant through every clap
    /// attribute would cost more than it buys; catching a stray `OLDNAME_FOO`
    /// or an unprefixed metric at test time buys the same thing for nothing.
    #[test]
    fn every_environment_variable_carries_the_prefix() {
        for name in env::ALL {
            assert!(
                name.starts_with(ENV_PREFIX),
                "`{name}` does not start with `{ENV_PREFIX}`"
            );
            assert_eq!(
                name.to_ascii_uppercase(),
                **name,
                "`{name}` should be upper case"
            );
        }
    }

    #[test]
    fn every_metric_series_carries_the_prefix() {
        for name in metrics::ALL {
            assert!(
                name.starts_with(METRIC_PREFIX),
                "`{name}` does not start with `{METRIC_PREFIX}`"
            );
            assert_eq!(
                name.to_ascii_lowercase(),
                **name,
                "`{name}` should be lower case"
            );
        }
    }

    #[test]
    fn the_prefixes_agree_with_the_binary_name_today() {
        // Not a rule, a tripwire: if a rebrand moves BIN without a deliberate
        // decision about the frozen names, this says so out loud.
        assert_eq!(ENV_PREFIX, format!("{}_", BIN.to_ascii_uppercase()));
        assert_eq!(METRIC_PREFIX, format!("{BIN}_"));
        assert!(CONFIG_FILE.starts_with(BIN));
        assert!(AUDIT_FILE.starts_with(BIN));
        assert!(APPROVALS_FILE.starts_with(BIN));
    }
}
