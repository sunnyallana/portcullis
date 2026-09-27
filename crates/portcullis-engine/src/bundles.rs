//! Versioned sets of actions, and which one a caller gets.
//!
//! Without this, changing what agents may do means editing the live
//! configuration and restarting: no canary, and no rollback that is not
//! another edit under pressure.
//!
//! **Versioning is per bundle, not per action.** An action's meaning depends
//! on the ones beside it — the same row filter, the same roles, the same
//! assumptions about the schema — so versioning them independently lets a
//! caller receive a combination nobody ever reasoned about. Rolling back is
//! also an operation on a set: you undo the change you shipped on Tuesday, not
//! one action out of it.
//!
//! A version is a label, not a number to compare. `3` does not need to follow
//! `2`, and nothing here sorts them: aliases decide what is live.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use portcullis_core::{Error, Result};

/// Which bundle version a request runs against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleId {
    /// The bundle's name, shared by every version of it.
    pub name: String,
    /// The version label.
    pub version: String,
}

impl BundleId {
    /// Build an id.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

impl std::fmt::Display for BundleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// A slice of traffic sent to an alias, chosen by hashing the caller.
///
/// Sticky on purpose: the same caller always lands on the same side, so a
/// canary that misbehaves does so consistently for the people affected rather
/// than intermittently for everyone. Random assignment would also make the
/// audit log impossible to reason about after the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canary {
    /// Alias the slice is sent to.
    pub alias: String,
    /// Percentage of callers, 1 to 99.
    pub percent: u8,
}

impl Canary {
    /// Does this caller fall in the slice?
    pub fn covers(&self, caller: &str) -> bool {
        bucket_of(caller) < u32::from(self.percent)
    }
}

/// Stable 0-99 bucket for a caller.
fn bucket_of(caller: &str) -> u32 {
    let mut h = Sha256::new();
    h.update(caller.as_bytes());
    let digest = h.finalize();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    n % 100
}

/// Aliases, the default, and any canary slice.
#[derive(Debug, Clone, Default)]
pub struct Routing {
    /// Alias name to version label.
    pub aliases: BTreeMap<String, String>,
    /// Alias used when a role names none.
    pub default: String,
    /// Optional slice of callers sent somewhere else.
    pub canary: Option<Canary>,
}

impl Routing {
    /// Resolve an alias, or accept a bare version label.
    ///
    /// Accepting a label directly is what lets an operator pin one role to an
    /// exact version while everyone else follows an alias.
    pub fn version_of<'a>(&'a self, wanted: &'a str) -> Option<&'a str> {
        self.aliases
            .get(wanted)
            .map(String::as_str)
            .or(Some(wanted))
    }

    /// Which version this caller and role should run against.
    ///
    /// A role that pins a version is never moved by the canary: pinning is how
    /// an operator says "not this one".
    pub fn resolve(&self, role_bundle: Option<&str>, caller: &str) -> String {
        if let Some(pinned) = role_bundle {
            return self.version_of(pinned).unwrap_or(pinned).to_owned();
        }
        if let Some(canary) = &self.canary {
            if canary.covers(caller) {
                if let Some(version) = self.aliases.get(&canary.alias) {
                    return version.clone();
                }
            }
        }
        self.version_of(&self.default)
            .unwrap_or(&self.default)
            .to_owned()
    }

    /// Check that every alias points at a version that exists.
    pub fn check(&self, available: &[String]) -> Result<()> {
        let known = |v: &str| available.iter().any(|a| a == v);

        for (alias, version) in &self.aliases {
            if !known(version) {
                return Err(Error::Config(format!(
                    "[bundles.alias] {alias} = \"{version}\" points at a version that is not loaded; loaded: {}",
                    available.join(", ")
                )));
            }
        }
        let default_version = self.version_of(&self.default).unwrap_or(&self.default);
        if !known(default_version) {
            return Err(Error::Config(format!(
                "[bundles] default = \"{}\" resolves to `{default_version}`, which is not loaded; loaded: {}",
                self.default,
                available.join(", ")
            )));
        }
        if let Some(canary) = &self.canary {
            if !self.aliases.contains_key(&canary.alias) {
                return Err(Error::Config(format!(
                    "[bundles.canary] alias = \"{}\" is not an alias",
                    canary.alias
                )));
            }
            if canary.percent == 0 || canary.percent >= 100 {
                return Err(Error::Config(
                    "[bundles.canary] percent must be between 1 and 99; 0 or 100 is a rollout, not a canary".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routing() -> Routing {
        Routing {
            aliases: BTreeMap::from([
                ("stable".to_string(), "2".to_string()),
                ("canary".to_string(), "3".to_string()),
            ]),
            default: "stable".into(),
            canary: Some(Canary {
                alias: "canary".into(),
                percent: 10,
            }),
        }
    }

    #[test]
    fn a_role_that_names_nothing_follows_the_default() {
        let r = Routing {
            canary: None,
            ..routing()
        };
        assert_eq!(r.resolve(None, "alice"), "2");
    }

    #[test]
    fn an_alias_resolves_and_a_bare_label_is_taken_as_is() {
        let r = routing();
        assert_eq!(r.resolve(Some("canary"), "alice"), "3");
        assert_eq!(
            r.resolve(Some("2"), "alice"),
            "2",
            "a pin may name a version"
        );
    }

    #[test]
    fn the_canary_is_sticky_per_caller() {
        let r = routing();
        for caller in ["alice", "bob", "carol", "dan", "erin"] {
            let first = r.resolve(None, caller);
            for _ in 0..20 {
                assert_eq!(
                    r.resolve(None, caller),
                    first,
                    "{caller} moved between calls"
                );
            }
        }
    }

    #[test]
    fn the_canary_covers_roughly_the_share_asked_for() {
        let r = routing();
        let sample = 4000;
        let hit = (0..sample)
            .filter(|i| r.resolve(None, &format!("caller-{i}")) == "3")
            .count();
        let share = hit * 100 / sample;
        assert!(
            (7..=13).contains(&share),
            "expected about 10% in the canary, got {share}%"
        );
    }

    #[test]
    fn a_pinned_role_is_never_moved_by_the_canary() {
        let r = routing();
        // Find a caller the canary would otherwise take.
        let taken = (0..1000)
            .map(|i| format!("caller-{i}"))
            .find(|c| r.resolve(None, c) == "3")
            .expect("some caller should fall in the slice");
        assert_eq!(
            r.resolve(Some("stable"), &taken),
            "2",
            "pinning must override the canary"
        );
    }

    #[test]
    fn an_alias_pointing_nowhere_is_caught_at_startup() {
        let mut r = routing();
        r.aliases.insert("next".into(), "9".into());
        let err = r.check(&["2".to_string(), "3".to_string()]).unwrap_err();
        assert!(format!("{err}").contains("not loaded"), "{err}");
    }

    #[test]
    fn a_canary_of_zero_or_a_hundred_is_refused() {
        for percent in [0u8, 100] {
            let r = Routing {
                canary: Some(Canary {
                    alias: "canary".into(),
                    percent,
                }),
                ..routing()
            };
            assert!(r.check(&["2".to_string(), "3".to_string()]).is_err());
        }
    }

    #[test]
    fn buckets_spread_across_the_range() {
        let seen: std::collections::BTreeSet<u32> =
            (0..500).map(|i| bucket_of(&format!("c{i}"))).collect();
        assert!(
            seen.len() > 80,
            "hashing should not clump: {} buckets",
            seen.len()
        );
    }
}
