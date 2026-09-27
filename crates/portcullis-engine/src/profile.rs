//! Reading a database and drafting a configuration for it.
//!
//! Writing actions by hand for a two-hundred-table schema is the thing most
//! likely to stop an evaluation before it starts. This reads the catalogue,
//! samples a few rows, guesses what each column holds, and emits a draft the
//! operator edits down.
//!
//! Two rules keep it honest. It only ever emits **read** actions, because a
//! guessed write is not a thing anyone should paste into production. And no
//! sampled value is printed in the clear: examples come out through the mask
//! that was suggested for them, so running the profiler cannot itself become
//! the leak.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use portcullis_core::branding;
use portcullis_core::{Caller, DataType, Mask, Result, Schema, Table, Value};
use portcullis_db::Backend;
use portcullis_db::plan::{ExecCtx, ReadPlan};

/// What a column appears to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Nothing sensitive spotted.
    Plain,
    /// Email address.
    Email,
    /// Telephone number.
    Phone,
    /// Payment card number.
    PaymentCard,
    /// National identifier: SSN, NI number, and the like.
    NationalId,
    /// Credential material.
    Secret,
    /// A person's name.
    PersonName,
    /// Postal address.
    PostalAddress,
    /// Bank account or IBAN.
    BankAccount,
    /// Date of birth.
    DateOfBirth,
}

impl Classification {
    /// A short label for the draft's comments, with its article.
    pub fn label(self) -> &'static str {
        match self {
            Self::Plain => "nothing sensitive",
            Self::Email => "an email address",
            Self::Phone => "a phone number",
            Self::PaymentCard => "a payment card",
            Self::NationalId => "a national identifier",
            Self::Secret => "a credential",
            Self::PersonName => "a person's name",
            Self::PostalAddress => "a postal address",
            Self::BankAccount => "a bank account",
            Self::DateOfBirth => "a date of birth",
        }
    }

    /// The mask this class deserves by default.
    pub fn suggested_mask(self) -> Mask {
        match self {
            Self::Plain => Mask::None,
            Self::Email | Self::PersonName | Self::PostalAddress => Mask::Partial,
            Self::Phone | Self::PaymentCard | Self::BankAccount => Mask::Last4,
            Self::NationalId | Self::Secret | Self::DateOfBirth => Mask::Redact,
        }
    }

    /// True when a human should look at this column before publishing.
    pub fn is_sensitive(self) -> bool {
        self != Self::Plain
    }
}

/// What the profiler found in one column.
#[derive(Debug, Clone)]
pub struct ColumnProfile {
    /// Column name.
    pub name: String,
    /// Declared type.
    pub ty: DataType,
    /// Whether the database allows NULL.
    pub nullable: bool,
    /// Nulls seen in the sample.
    pub nulls: usize,
    /// Distinct values seen in the sample.
    pub distinct: usize,
    /// What it looks like.
    pub classification: Classification,
    /// Why it was classified that way.
    pub evidence: &'static str,
    /// One masked example, or `None` when the sample was empty.
    pub example: Option<String>,
}

/// What the profiler found in one table.
#[derive(Debug, Clone)]
pub struct TableProfile {
    /// Qualified name.
    pub name: String,
    /// Primary key columns.
    pub primary_key: Vec<String>,
    /// Columns, in declaration order.
    pub columns: Vec<ColumnProfile>,
    /// Rows sampled.
    pub sampled: usize,
    /// Columns that look like a tenant or region boundary.
    pub scope_candidates: Vec<String>,
}

impl TableProfile {
    /// The bare table name, for naming actions.
    pub fn short_name(&self) -> &str {
        self.name
            .rsplit_once('.')
            .map_or(self.name.as_str(), |(_, n)| n)
    }

    /// Columns worth masking.
    pub fn masked(&self) -> Vec<&ColumnProfile> {
        self.columns
            .iter()
            .filter(|c| c.classification.is_sensitive())
            .collect()
    }
}

/// A whole profiled database.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Tables, in catalogue order.
    pub tables: Vec<TableProfile>,
}

/// Read the catalogue, sample rows, classify columns.
pub async fn profile(backend: &dyn Backend, schema: &Schema, sample: u32) -> Result<Profile> {
    // The profiler reads with no scope of its own; it is an operator tool, run
    // deliberately, and its output is a draft rather than a published action.
    let caller = Caller::new(branding::PROFILE_CALLER, "profiler");
    let args = BTreeMap::new();

    let mut tables = Vec::new();
    for table in schema.tables.values() {
        tables.push(profile_table(backend, table, sample, &caller, &args).await?);
    }
    Ok(Profile { tables })
}

async fn profile_table(
    backend: &dyn Backend,
    table: &Table,
    sample: u32,
    caller: &Caller,
    args: &BTreeMap<String, Value>,
) -> Result<TableProfile> {
    let columns: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();

    let rows = if columns.is_empty() || sample == 0 {
        portcullis_db::plan::Rows::default()
    } else {
        backend
            .read(
                &ReadPlan {
                    table: &table.name,
                    columns: &columns,
                    filter: None,
                    row_filter: None,
                    order_by: &[],
                    limit: sample,
                },
                &ExecCtx {
                    args,
                    caller,
                    timeout: std::time::Duration::from_secs(30),
                },
            )
            .await
            // A table the profiler cannot read is reported as unsampled rather
            // than failing the whole run: permissions vary per table.
            .unwrap_or_default()
    };

    let mut profiles = Vec::new();
    for (index, column) in table.columns.iter().enumerate() {
        let values: Vec<&Value> = rows.rows.iter().filter_map(|r| r.get(index)).collect();
        let nulls = values.iter().filter(|v| v.is_null()).count();
        let mut seen: Vec<String> = values
            .iter()
            .filter(|v| !v.is_null())
            .map(ToString::to_string)
            .collect();
        seen.sort_unstable();
        let distinct = {
            let mut d = seen.clone();
            d.dedup();
            d.len()
        };

        let (classification, evidence) = classify(&column.name, column.ty, &seen);
        let example = seen.first().map(|v| {
            classification
                .suggested_mask()
                .apply(&Value::Text(v.clone()), "profile")
                .to_string()
        });

        profiles.push(ColumnProfile {
            name: column.name.clone(),
            ty: column.ty,
            nullable: column.nullable,
            nulls,
            distinct,
            classification,
            evidence,
            example,
        });
    }

    let scope_candidates = profiles
        .iter()
        .filter(|c| looks_like_scope(&c.name) && c.ty == DataType::Text)
        .map(|c| c.name.clone())
        .collect();

    Ok(TableProfile {
        name: table.name.clone(),
        primary_key: table.primary_key.clone(),
        columns: profiles,
        sampled: rows.len(),
        scope_candidates,
    })
}

/// Guess what a column holds, from its name and then its contents.
///
/// The name is checked first because it is the stronger signal in practice: a
/// column called `customer_email` holding nulls in the sample is still an
/// email column.
fn classify(name: &str, ty: DataType, values: &[String]) -> (Classification, &'static str) {
    let lower = name.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    if has(&[
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "token",
        "private_key",
    ]) {
        return (Classification::Secret, "the column name");
    }
    if has(&["email", "e_mail", "mail_address"]) {
        return (Classification::Email, "the column name");
    }
    if has(&[
        "ssn",
        "social_security",
        "national_id",
        "nino",
        "tax_id",
        "passport",
    ]) {
        return (Classification::NationalId, "the column name");
    }
    if has(&[
        "iban",
        "bic",
        "swift",
        "account_number",
        "sort_code",
        "routing",
    ]) {
        return (Classification::BankAccount, "the column name");
    }
    if has(&["card", "pan", "cc_num", "credit_card"]) {
        return (Classification::PaymentCard, "the column name");
    }
    if has(&["phone", "mobile", "telephone", "msisdn"]) {
        return (Classification::Phone, "the column name");
    }
    if has(&["date_of_birth", "birth_date", "dob", "birthday"]) {
        return (Classification::DateOfBirth, "the column name");
    }
    if has(&["address", "street", "postcode", "postal_code", "zip"]) {
        return (Classification::PostalAddress, "the column name");
    }
    if has(&[
        "first_name",
        "last_name",
        "surname",
        "full_name",
        "given_name",
        "family_name",
    ]) || lower == "name"
    {
        return (Classification::PersonName, "the column name");
    }

    // Nothing in the name, so look at what is actually in there.
    if ty == DataType::Text && !values.is_empty() {
        let n = values.len();
        let emails = values.iter().filter(|v| looks_like_email(v)).count();
        if emails * 2 > n {
            return (Classification::Email, "the sampled values");
        }
        let cards = values.iter().filter(|v| looks_like_card(v)).count();
        if cards * 2 > n {
            return (Classification::PaymentCard, "the sampled values");
        }
        let phones = values.iter().filter(|v| looks_like_phone(v)).count();
        if phones * 2 > n {
            return (Classification::Phone, "the sampled values");
        }
    }

    (Classification::Plain, "")
}

fn looks_like_email(v: &str) -> bool {
    let bytes = v.as_bytes();
    if bytes.len() < 6 || bytes.len() > 254 {
        return false;
    }
    let Some((local, domain)) = v.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !v.contains(' ')
}

/// Digits of a plausible card length that pass the Luhn check.
///
/// The Luhn test is what keeps order numbers and account ids from being
/// flagged as cards, which would push people to ignore the profiler's output.
fn looks_like_card(v: &str) -> bool {
    let digits: Vec<u32> = v.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    if v.chars()
        .any(|c| !c.is_ascii_digit() && c != ' ' && c != '-')
    {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                *d
            }
        })
        .sum();
    sum % 10 == 0
}

fn looks_like_phone(v: &str) -> bool {
    let digits = v.chars().filter(char::is_ascii_digit).count();
    let allowed = v
        .chars()
        .all(|c| c.is_ascii_digit() || " +-()./".contains(c));
    allowed && (7..=15).contains(&digits) && v.len() <= 24
}

fn looks_like_scope(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "region",
        "tenant",
        "tenant_id",
        "org",
        "org_id",
        "organisation",
        "organization",
        "customer_id",
        "account_id",
        "workspace",
        "site",
        "country",
    ]
    .iter()
    .any(|c| lower == *c)
}

impl Profile {
    /// Render a draft configuration.
    ///
    /// The result is valid TOML and deliberately conservative: reads only, a
    /// row limit on everything, masks pre-filled, and a commented row filter
    /// wherever a scope column was spotted.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear template; splitting it hides the shape of the output"
    )]
    pub fn to_toml(&self, server_name: &str) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# Drafted by `{} profile`. Read it before you serve it.",
            branding::BIN
        );
        out.push_str("#\n");
        out.push_str("# Every action here is a read with a row limit. Masks were suggested from\n");
        out.push_str("# column names and sampled values; check them. Where a scope column was\n");
        out.push_str("# found, a row_filter is written but commented out, because only you know\n");
        out.push_str("# which attribute your callers carry.\n\n");

        let _ = writeln!(
            out,
            "[server]\nname = \"{server_name}\"\nmask_salt = \"env:{}MASK_SALT\"\n",
            branding::ENV_PREFIX
        );
        out.push_str("[backend]\nkind = \"postgres\"\ndsn = \"env:DATABASE_URL\"\n\n");
        let _ = writeln!(
            out,
            "[audit]\npath = \"{}\"\nfsync = \"always\"\n",
            branding::AUDIT_FILE
        );
        out.push_str("[[role]]\nname = \"reader\"\nallow = [\"*\"]\n");
        out.push_str("# attributes = { region = \"EU\" }\n\n");

        for table in &self.tables {
            let short = table.short_name();
            let _ = writeln!(
                out,
                "# ---------------------------------------------------------------"
            );
            let _ = writeln!(
                out,
                "# {} — {} column(s), {} row(s) sampled",
                table.name,
                table.columns.len(),
                table.sampled
            );
            for column in &table.columns {
                if column.classification.is_sensitive() {
                    let _ = writeln!(
                        out,
                        "#   {} looks like {} (from {}){}",
                        column.name,
                        column.classification.label(),
                        column.evidence,
                        column
                            .example
                            .as_ref()
                            .map_or(String::new(), |e| format!(", e.g. {e}"))
                    );
                }
            }
            let _ = writeln!(
                out,
                "# ---------------------------------------------------------------\n"
            );

            let returns = table
                .columns
                .iter()
                .filter(|c| c.classification != Classification::Secret)
                .map(|c| format!("\"{}\"", c.name))
                .collect::<Vec<_>>()
                .join(", ");
            let masks = table
                .masked()
                .iter()
                .filter(|c| c.classification != Classification::Secret)
                .map(|c| {
                    format!(
                        "{} = \"{}\"",
                        c.name,
                        mask_name(c.classification.suggested_mask())
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            if let Some(key) = table.primary_key.first() {
                let key_type = table
                    .columns
                    .iter()
                    .find(|c| &c.name == key)
                    .map_or("text", |c| c.ty.name());
                let _ = writeln!(out, "[action.find_{short}]");
                let _ = writeln!(
                    out,
                    "description = \"Look up one row of {short} by {key}.\""
                );
                let _ = writeln!(out, "table = \"{}\"", table.name);
                let _ = writeln!(
                    out,
                    "params = {{ {key} = {{ type = \"{key_type}\", required = true }} }}"
                );
                let _ = writeln!(out, "returns = [{returns}]");
                let _ = writeln!(out, "filter = \"{key} = :{key}\"");
                if let Some(scope) = table.scope_candidates.first() {
                    let _ = writeln!(out, "# row_filter = \"{scope} = $caller.{scope}\"");
                }
                if !masks.is_empty() {
                    let _ = writeln!(out, "mask = {{ {masks} }}");
                }
                let _ = writeln!(out, "max_rows = 1\n");
            }

            let _ = writeln!(out, "[action.list_{short}]");
            let _ = writeln!(out, "description = \"List rows of {short}.\"");
            let _ = writeln!(out, "table = \"{}\"", table.name);
            let _ = writeln!(out, "returns = [{returns}]");
            if let Some(scope) = table.scope_candidates.first() {
                let _ = writeln!(out, "# row_filter = \"{scope} = $caller.{scope}\"");
            }
            if !masks.is_empty() {
                let _ = writeln!(out, "mask = {{ {masks} }}");
            }
            let _ = writeln!(out, "max_rows = 50\n");
        }

        out
    }

    /// A short human summary of what was found.
    pub fn summary(&self) -> String {
        let sensitive: usize = self.tables.iter().map(|t| t.masked().len()).sum();
        let columns: usize = self.tables.iter().map(|t| t.columns.len()).sum();
        format!(
            "{} table(s), {columns} column(s), {sensitive} needing a mask",
            self.tables.len()
        )
    }
}

fn mask_name(mask: Mask) -> &'static str {
    match mask {
        Mask::None => "none",
        Mask::Partial => "partial",
        Mask::Last4 => "last4",
        Mask::Hash => "hash",
        Mask::Redact => "redact",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_classify_the_obvious_cases() {
        assert_eq!(
            classify("customer_email", DataType::Text, &[]).0,
            Classification::Email
        );
        assert_eq!(
            classify("card_last4", DataType::Text, &[]).0,
            Classification::PaymentCard
        );
        assert_eq!(
            classify("api_key", DataType::Text, &[]).0,
            Classification::Secret
        );
        assert_eq!(
            classify("date_of_birth", DataType::Timestamp, &[]).0,
            Classification::DateOfBirth
        );
        assert_eq!(
            classify("order_no", DataType::Text, &[]).0,
            Classification::Plain
        );
    }

    #[test]
    fn values_classify_what_names_hide() {
        let values = vec![
            "alice@example.com".to_string(),
            "bob@example.co.uk".to_string(),
        ];
        assert_eq!(
            classify("contact", DataType::Text, &values).0,
            Classification::Email
        );
    }

    #[test]
    fn order_numbers_are_not_mistaken_for_cards() {
        // Sixteen digits, but not a valid Luhn sequence.
        let values = vec![
            "1234567890123456".to_string(),
            "1234567890123457".to_string(),
        ];
        assert_eq!(
            classify("reference", DataType::Text, &values).0,
            Classification::Plain
        );

        // A real test card number does pass.
        let cards = vec![
            "4111111111111111".to_string(),
            "5555555555554444".to_string(),
        ];
        assert_eq!(
            classify("reference", DataType::Text, &cards).0,
            Classification::PaymentCard
        );
    }

    #[test]
    fn short_numbers_are_not_phones() {
        assert!(!looks_like_phone("8812"));
        assert!(looks_like_phone("+44 20 7946 0958"));
        assert!(!looks_like_phone("this is not a phone number at all"));
    }

    #[test]
    fn suggested_masks_match_the_class() {
        assert_eq!(Classification::Email.suggested_mask(), Mask::Partial);
        assert_eq!(Classification::PaymentCard.suggested_mask(), Mask::Last4);
        assert_eq!(Classification::Secret.suggested_mask(), Mask::Redact);
        assert_eq!(Classification::Plain.suggested_mask(), Mask::None);
    }

    #[test]
    fn scope_columns_are_spotted_by_exact_name() {
        assert!(looks_like_scope("region"));
        assert!(looks_like_scope("tenant_id"));
        assert!(!looks_like_scope("regional_manager"));
    }

    #[test]
    fn the_draft_is_valid_toml_and_holds_no_clear_values() {
        let profile = Profile {
            tables: vec![TableProfile {
                name: "public.customers".into(),
                primary_key: vec!["id".into()],
                columns: vec![
                    ColumnProfile {
                        name: "id".into(),
                        ty: DataType::Int,
                        nullable: false,
                        nulls: 0,
                        distinct: 3,
                        classification: Classification::Plain,
                        evidence: "",
                        example: Some("1".into()),
                    },
                    ColumnProfile {
                        name: "email".into(),
                        ty: DataType::Text,
                        nullable: false,
                        nulls: 0,
                        distinct: 3,
                        classification: Classification::Email,
                        evidence: "the column name",
                        example: Some("a***@example.com".into()),
                    },
                    ColumnProfile {
                        name: "region".into(),
                        ty: DataType::Text,
                        nullable: false,
                        nulls: 0,
                        distinct: 2,
                        classification: Classification::Plain,
                        evidence: "",
                        example: Some("EU".into()),
                    },
                ],
                sampled: 3,
                scope_candidates: vec!["region".into()],
            }],
        };

        let draft = profile.to_toml("drafted");
        let parsed: toml::Value = toml::from_str(&draft).expect("the draft should be valid TOML");
        assert!(parsed.get("action").is_some());
        assert!(draft.contains("mask = { email = \"partial\" }"));
        assert!(draft.contains("# row_filter = \"region = $caller.region\""));
        assert!(
            !draft.contains("alice@example.com"),
            "no clear values in the draft"
        );
        assert_eq!(
            profile.summary(),
            "1 table(s), 3 column(s), 1 needing a mask"
        );
    }

    #[test]
    fn secrets_are_left_out_of_returns_entirely() {
        let profile = Profile {
            tables: vec![TableProfile {
                name: "public.tokens".into(),
                primary_key: vec![],
                columns: vec![
                    ColumnProfile {
                        name: "owner".into(),
                        ty: DataType::Text,
                        nullable: false,
                        nulls: 0,
                        distinct: 1,
                        classification: Classification::Plain,
                        evidence: "",
                        example: None,
                    },
                    ColumnProfile {
                        name: "api_key".into(),
                        ty: DataType::Text,
                        nullable: false,
                        nulls: 0,
                        distinct: 1,
                        classification: Classification::Secret,
                        evidence: "the column name",
                        example: Some("[redacted]".into()),
                    },
                ],
                sampled: 1,
                scope_candidates: vec![],
            }],
        };
        let draft = profile.to_toml("s");
        assert!(draft.contains("returns = [\"owner\"]"));
        assert!(
            !draft.contains("\"api_key\""),
            "a credential column is not exposed at all"
        );
    }
}
