//! Column masking.
//!
//! Masking runs on the way out, after the database has answered and before the
//! value reaches the agent or the audit log. Doing it here rather than in SQL
//! means a filter can still match on the real value while the model only ever
//! sees the masked form.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::value::Value;

/// How to obscure a column before it leaves the process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mask {
    /// Return the value untouched.
    #[default]
    None,
    /// Keep the shape, hide the middle. `alice@example.com` becomes `a***@example.com`.
    Partial,
    /// Keep the final four characters. `4111111111111111` becomes `************1111`.
    Last4,
    /// Replace with a stable salted digest, so equal values stay equal.
    Hash,
    /// Replace with a fixed marker.
    Redact,
}

impl Mask {
    /// Parse the spelling used in configuration files.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Self::None,
            "partial" => Self::Partial,
            "last4" => Self::Last4,
            "hash" => Self::Hash,
            "redact" | "hide" => Self::Redact,
            _ => return None,
        })
    }

    /// Apply the mask.
    ///
    /// `salt` keeps [`Mask::Hash`] digests from being reversible with a rainbow
    /// table; it comes from the deployment config and stays on the server.
    pub fn apply(self, value: &Value, salt: &str) -> Value {
        if matches!(self, Self::None) || value.is_null() {
            return value.clone();
        }
        match self {
            Self::None => value.clone(),
            Self::Redact => Value::Text("[redacted]".into()),
            Self::Hash => {
                let mut h = Sha256::new();
                h.update(salt.as_bytes());
                h.update(b"\x1f");
                h.update(value.to_string().as_bytes());
                let digest = crate::hex::encode(&h.finalize());
                Value::Text(format!("sha256:{}", &digest[..16]))
            }
            Self::Partial => Value::Text(partial(&value.to_string())),
            Self::Last4 => Value::Text(last4(&value.to_string())),
        }
    }
}

/// Hide the middle of a string, keeping enough shape to be recognisable.
///
/// Email addresses keep their domain, because "which customer" is usually the
/// question and the domain rarely is the secret.
fn partial(s: &str) -> String {
    if let Some((local, domain)) = s.split_once('@') {
        let head = local.chars().next().map(String::from).unwrap_or_default();
        return format!("{head}***@{domain}");
    }
    let chars: Vec<char> = s.chars().collect();
    match chars.len() {
        0 => String::new(),
        1..=2 => "*".repeat(chars.len()),
        n => format!("{}{}{}", chars[0], "*".repeat(n - 2), chars[n - 1]),
    }
}

/// Keep the last four characters, star the rest.
fn last4(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}{tail}", "*".repeat(chars.len() - 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_keeps_its_domain() {
        let v = Value::Text("alice@example.com".into());
        assert_eq!(
            Mask::Partial.apply(&v, "salt"),
            Value::Text("a***@example.com".into())
        );
    }

    #[test]
    fn card_numbers_keep_four_digits() {
        let v = Value::Text("4111111111111111".into());
        assert_eq!(
            Mask::Last4.apply(&v, "salt"),
            Value::Text("************1111".into())
        );
    }

    #[test]
    fn hash_is_stable_and_salted() {
        let v = Value::Text("alice@example.com".into());
        assert_eq!(Mask::Hash.apply(&v, "s1"), Mask::Hash.apply(&v, "s1"));
        assert_ne!(Mask::Hash.apply(&v, "s1"), Mask::Hash.apply(&v, "s2"));
    }

    #[test]
    fn null_is_never_disguised_as_a_value() {
        for m in [Mask::Partial, Mask::Hash, Mask::Redact, Mask::Last4] {
            assert_eq!(m.apply(&Value::Null, "salt"), Value::Null);
        }
    }

    #[test]
    fn short_strings_do_not_leak_through_partial() {
        assert_eq!(partial("ab"), "**");
        assert_eq!(partial("a"), "*");
    }
}
