//! Who is asking.
//!
//! Every request carries a `Caller`. Its attributes are the only values a row
//! filter may reference through `$caller.*`, which is what makes tenant and
//! region scoping impossible for an agent to talk its way around: the values
//! come from the deployment's identity configuration, never from tool
//! arguments.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::value::Value;

/// The identity a request runs as.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caller {
    /// Stable identifier recorded in the audit log.
    pub id: String,
    /// Role name, matched against the `[[role]]` blocks in the config.
    pub role: String,
    /// Attributes available to row filters as `$caller.<key>`.
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
}

impl Caller {
    /// A caller with no attributes.
    pub fn new(id: impl Into<String>, role: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            role: role.into(),
            attributes: BTreeMap::new(),
        }
    }

    /// Attach an attribute that row filters can reference.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    /// Look up an attribute referenced by `$caller.<key>`.
    pub fn attribute(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }

    /// Resolve `$caller.<key>`.
    ///
    /// `id` and `role` are always available and cannot be shadowed by a
    /// configured attribute, so `issued_by = "$caller.id"` records the real
    /// caller rather than whatever a role happened to declare.
    pub fn lookup(&self, key: &str) -> Option<Value> {
        match key {
            "id" => Some(Value::Text(self.id.clone())),
            "role" => Some(Value::Text(self.role.clone())),
            other => self.attributes.get(other).cloned(),
        }
    }

    /// Keys that are always available, whatever the role declares.
    pub const BUILT_IN: &'static [&'static str] = &["id", "role"];
}
