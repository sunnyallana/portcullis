//! The predicate language operators write in `filter` and `row_filter`.
//!
//! The grammar is small on purpose. The left side of every comparison is a bare
//! column name; the right side is a bound parameter, a caller attribute or a
//! literal. There are no functions, no arithmetic, no subqueries and no way to
//! name a second table. Anything an operator writes therefore compiles to a
//! predicate over one table with every value bound, and an agent's arguments
//! can only ever become bind parameters.
//!
//! ```text
//! expr      := or
//! or        := and ("or" and)*
//! and       := unary ("and" unary)*
//! unary     := "not" unary | "(" expr ")" | predicate
//! predicate := column op term
//!            | column "is" ["not"] "null"
//!            | column ["not"] "in" "(" term {"," term} ")"
//! op        := "=" | "!=" | "<>" | "<" | "<=" | ">" | ">=" | "like"
//! term      := ":" name | "$caller." name | number | 'text' | true | false | null
//! ```

use std::collections::BTreeSet;
use std::fmt;

use rust_decimal::Decimal;
use sluice_core::{Error, Result, Value};

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `!=` or `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `like`
    Like,
}

impl CmpOp {
    /// The SQL spelling.
    pub fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Like => "LIKE",
        }
    }
}

/// The right-hand side of a comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    /// `:name`, supplied by the caller.
    Param(String),
    /// `$caller.name`, supplied by the identity layer.
    Caller(String),
    /// A literal written in the configuration file.
    Lit(Value),
    /// `now()`
    Now,
    /// `uuid()`
    NewUuid,
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Param(p) => write!(f, ":{p}"),
            Self::Caller(a) => write!(f, "$caller.{a}"),
            Self::Lit(v) => write!(f, "{v}"),
            Self::Now => f.write_str("now()"),
            Self::NewUuid => f.write_str("uuid()"),
        }
    }
}

/// A parsed predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Both sides must hold.
    And(Box<Expr>, Box<Expr>),
    /// Either side may hold.
    Or(Box<Expr>, Box<Expr>),
    /// Negation.
    Not(Box<Expr>),
    /// `column op term`
    Cmp {
        /// Column on the left.
        column: String,
        /// Operator.
        op: CmpOp,
        /// Term on the right.
        term: Term,
    },
    /// `column is [not] null`
    IsNull {
        /// Column tested.
        column: String,
        /// `is not null` when true.
        negated: bool,
    },
    /// `column [not] in (terms...)`
    In {
        /// Column tested.
        column: String,
        /// `not in` when true.
        negated: bool,
        /// Candidate terms.
        terms: Vec<Term>,
    },
}

impl Expr {
    /// Parse a predicate.
    pub fn parse(src: &str) -> Result<Self> {
        let tokens = lex(src)?;
        let mut p = Parser { tokens, pos: 0 };
        let e = p.parse_or()?;
        p.expect_end()?;
        Ok(e)
    }

    /// Every column this predicate touches.
    pub fn columns(&self) -> BTreeSet<&str> {
        let mut out = BTreeSet::new();
        self.walk(&mut |e| match e {
            Self::Cmp { column, .. } | Self::IsNull { column, .. } | Self::In { column, .. } => {
                out.insert(column.as_str());
            }
            _ => {}
        });
        out
    }

    /// Every `:parameter` this predicate references.
    pub fn params(&self) -> BTreeSet<&str> {
        let mut out = BTreeSet::new();
        self.walk(&mut |e| {
            for t in e.terms() {
                if let Term::Param(p) = t {
                    out.insert(p.as_str());
                }
            }
        });
        out
    }

    /// Every `$caller.attribute` this predicate references.
    pub fn caller_attributes(&self) -> BTreeSet<&str> {
        let mut out = BTreeSet::new();
        self.walk(&mut |e| {
            for t in e.terms() {
                if let Term::Caller(a) = t {
                    out.insert(a.as_str());
                }
            }
        });
        out
    }

    /// The terms appearing directly in this node.
    fn terms(&self) -> Vec<&Term> {
        match self {
            Self::Cmp { term, .. } => vec![term],
            Self::In { terms, .. } => terms.iter().collect(),
            _ => Vec::new(),
        }
    }

    fn walk<'a>(&'a self, f: &mut impl FnMut(&'a Self)) {
        f(self);
        match self {
            Self::And(a, b) | Self::Or(a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Self::Not(a) => a.walk(f),
            _ => {}
        }
    }

    /// Combine two predicates with AND, keeping either when the other is absent.
    pub fn and_opt(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(x), Some(y)) => Some(Self::And(Box::new(x), Box::new(y))),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Param(String),
    Caller(String),
    Int(i64),
    Dec(Decimal),
    Str(String),
    Op(CmpOp),
    LParen,
    RParen,
    Comma,
    And,
    Or,
    Not,
    Is,
    Null,
    In,
    True,
    False,
    Now,
    UuidFn,
}

#[allow(
    clippy::too_many_lines,
    reason = "a lexer is one match over the alphabet; splitting it hides the grammar"
)]
fn lex(src: &str) -> Result<Vec<(Tok, usize)>> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let start = i;
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let tok = match c {
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            '=' => {
                i += 1;
                Tok::Op(CmpOp::Eq)
            }
            '!' if b.get(i + 1) == Some(&'=') => {
                i += 2;
                Tok::Op(CmpOp::Ne)
            }
            '<' if b.get(i + 1) == Some(&'>') => {
                i += 2;
                Tok::Op(CmpOp::Ne)
            }
            '<' if b.get(i + 1) == Some(&'=') => {
                i += 2;
                Tok::Op(CmpOp::Le)
            }
            '>' if b.get(i + 1) == Some(&'=') => {
                i += 2;
                Tok::Op(CmpOp::Ge)
            }
            '<' => {
                i += 1;
                Tok::Op(CmpOp::Lt)
            }
            '>' => {
                i += 1;
                Tok::Op(CmpOp::Gt)
            }
            ':' => {
                i += 1;
                let name = take_ident(&b, &mut i);
                if name.is_empty() {
                    return Err(parse_err(start, "expected a parameter name after `:`"));
                }
                Tok::Param(name)
            }
            '$' => {
                i += 1;
                let head = take_ident(&b, &mut i);
                if !head.eq_ignore_ascii_case("caller") || b.get(i) != Some(&'.') {
                    return Err(parse_err(
                        start,
                        "the only variable available is `$caller.<attribute>`",
                    ));
                }
                i += 1;
                let attr = take_ident(&b, &mut i);
                if attr.is_empty() {
                    return Err(parse_err(
                        start,
                        "expected an attribute name after `$caller.`",
                    ));
                }
                Tok::Caller(attr)
            }
            '\'' => {
                i += 1;
                let mut s = String::new();
                loop {
                    match b.get(i) {
                        None => return Err(parse_err(start, "unterminated text literal")),
                        Some('\'') if b.get(i + 1) == Some(&'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(ch) => {
                            s.push(*ch);
                            i += 1;
                        }
                    }
                }
                Tok::Str(s)
            }
            c if c.is_ascii_digit()
                || (c == '-' && b.get(i + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let mut s = String::new();
                if c == '-' {
                    s.push('-');
                    i += 1;
                }
                let mut seen_dot = false;
                while let Some(ch) = b.get(i) {
                    if ch.is_ascii_digit() {
                        s.push(*ch);
                        i += 1;
                    } else if *ch == '.' && !seen_dot {
                        seen_dot = true;
                        s.push('.');
                        i += 1;
                    } else {
                        break;
                    }
                }
                if seen_dot {
                    Tok::Dec(
                        Decimal::from_str_exact(&s)
                            .map_err(|e| parse_err(start, &format!("bad number: {e}")))?,
                    )
                } else {
                    Tok::Int(
                        s.parse()
                            .map_err(|_| parse_err(start, "number does not fit in 64 bits"))?,
                    )
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let word = take_ident(&b, &mut i);
                match word.to_ascii_lowercase().as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "is" => Tok::Is,
                    "null" => Tok::Null,
                    "in" => Tok::In,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "like" => Tok::Op(CmpOp::Like),
                    "now" if peek_call(&b, i) => {
                        i += 2;
                        Tok::Now
                    }
                    "uuid" if peek_call(&b, i) => {
                        i += 2;
                        Tok::UuidFn
                    }
                    _ => Tok::Ident(word),
                }
            }
            other => {
                return Err(parse_err(
                    start,
                    &format!("`{other}` cannot appear in a predicate"),
                ));
            }
        };
        out.push((tok, start));
    }
    Ok(out)
}

fn peek_call(b: &[char], i: usize) -> bool {
    b.get(i) == Some(&'(') && b.get(i + 1) == Some(&')')
}

fn take_ident(b: &[char], i: &mut usize) -> String {
    let mut s = String::new();
    while let Some(c) = b.get(*i) {
        if c.is_ascii_alphanumeric() || *c == '_' {
            s.push(*c);
            *i += 1;
        } else {
            break;
        }
    }
    s
}

fn parse_err(at: usize, problem: &str) -> Error {
    Error::Parse {
        at,
        problem: problem.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<(Tok, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn at(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map_or_else(|| self.tokens.last().map_or(0, |(_, p)| *p), |(_, p)| *p)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect_end(&self) -> Result<()> {
        if self.pos == self.tokens.len() {
            Ok(())
        } else {
            Err(parse_err(self.at(), "unexpected trailing input"))
        }
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.eat(&Tok::Or) {
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        while self.eat(&Tok::And) {
            let right = self.parse_unary()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Not) {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat(&Tok::LParen) {
            let e = self.parse_or()?;
            if !self.eat(&Tok::RParen) {
                return Err(parse_err(self.at(), "expected `)`"));
            }
            return Ok(e);
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Expr> {
        let at = self.at();
        let column = match self.next() {
            Some(Tok::Ident(name)) => name,
            Some(Tok::Param(p)) => {
                return Err(parse_err(
                    at,
                    &format!(
                        "`:{p}` is on the left of a comparison; a predicate must start with a column name"
                    ),
                ));
            }
            _ => return Err(parse_err(at, "expected a column name")),
        };

        if self.eat(&Tok::Is) {
            let negated = self.eat(&Tok::Not);
            if !self.eat(&Tok::Null) {
                return Err(parse_err(self.at(), "expected `null` after `is`"));
            }
            return Ok(Expr::IsNull { column, negated });
        }

        let negated = self.eat(&Tok::Not);
        if self.eat(&Tok::In) {
            if !self.eat(&Tok::LParen) {
                return Err(parse_err(self.at(), "expected `(` after `in`"));
            }
            let mut terms = Vec::new();
            loop {
                terms.push(self.parse_term()?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                if self.eat(&Tok::RParen) {
                    break;
                }
                return Err(parse_err(self.at(), "expected `,` or `)`"));
            }
            return Ok(Expr::In {
                column,
                negated,
                terms,
            });
        }
        if negated {
            return Err(parse_err(self.at(), "expected `in` or `null` after `not`"));
        }

        let Some(Tok::Op(op)) = self.next() else {
            return Err(parse_err(
                self.at(),
                "expected a comparison operator (=, !=, <, <=, >, >=, like, is null, in)",
            ));
        };
        let term = self.parse_term()?;
        Ok(Expr::Cmp { column, op, term })
    }

    fn parse_term(&mut self) -> Result<Term> {
        let at = self.at();
        Ok(match self.next() {
            Some(Tok::Param(p)) => Term::Param(p),
            Some(Tok::Caller(a)) => Term::Caller(a),
            Some(Tok::Int(i)) => Term::Lit(Value::Int(i)),
            Some(Tok::Dec(d)) => Term::Lit(Value::Decimal(d)),
            Some(Tok::Str(s)) => Term::Lit(Value::Text(s)),
            Some(Tok::True) => Term::Lit(Value::Bool(true)),
            Some(Tok::False) => Term::Lit(Value::Bool(false)),
            Some(Tok::Null) => Term::Lit(Value::Null),
            Some(Tok::Now) => Term::Now,
            Some(Tok::UuidFn) => Term::NewUuid,
            Some(Tok::Ident(w)) => {
                return Err(parse_err(
                    at,
                    &format!(
                        "`{w}` is not a value; comparing two columns is not supported, and text literals need single quotes"
                    ),
                ));
            }
            _ => return Err(parse_err(at, "expected a value")),
        })
    }
}

/// Parse a standalone term, as used by `write.columns`.
pub fn parse_term(src: &str) -> Result<Term> {
    let tokens = lex(src)?;
    let mut p = Parser { tokens, pos: 0 };
    let t = p.parse_term()?;
    p.expect_end()?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn and_binds_tighter_than_or() {
        let e = Expr::parse("a = 1 or b = 2 and c = 3").unwrap();
        // Parsed as a OR (b AND c).
        assert!(matches!(e, Expr::Or(_, r) if matches!(*r, Expr::And(..))));
    }

    #[test]
    fn a_predicate_cannot_start_with_a_parameter() {
        let err = Expr::parse(":order_no = order_no").unwrap_err();
        assert!(
            format!("{err}").contains("must start with a column name"),
            "{err}"
        );
    }

    #[test]
    fn column_to_column_comparison_is_refused() {
        let err = Expr::parse("total = discount").unwrap_err();
        assert!(format!("{err}").contains("comparing two columns"), "{err}");
    }

    #[test]
    fn only_caller_is_a_variable() {
        let err = Expr::parse("region = $env.region").unwrap_err();
        assert!(format!("{err}").contains("$caller."), "{err}");
    }

    #[test]
    fn text_literals_handle_embedded_quotes() {
        let e = Expr::parse("name = 'O''Brien'").unwrap();
        assert_eq!(
            e,
            Expr::Cmp {
                column: "name".into(),
                op: CmpOp::Eq,
                term: Term::Lit(Value::Text("O'Brien".into())),
            }
        );
    }

    #[test]
    fn a_semicolon_is_not_part_of_the_language() {
        let err = Expr::parse("order_no = :order_no; drop table orders").unwrap_err();
        assert!(format!("{err}").contains("cannot appear"), "{err}");
    }

    #[test]
    fn comment_syntax_is_not_part_of_the_language() {
        assert!(Expr::parse("order_no = :o -- and 1=1").is_err());
        assert!(Expr::parse("order_no = :o /* x */").is_err());
    }

    #[test]
    fn collects_columns_params_and_caller_attributes() {
        let e = Expr::parse(
            "status in ('open','held') and region = $caller.region and placed_at >= :since",
        )
        .unwrap();
        assert_eq!(
            e.columns().into_iter().collect::<Vec<_>>(),
            vec!["placed_at", "region", "status"]
        );
        assert_eq!(e.params().into_iter().collect::<Vec<_>>(), vec!["since"]);
        assert_eq!(
            e.caller_attributes().into_iter().collect::<Vec<_>>(),
            vec!["region"]
        );
    }

    #[test]
    fn is_not_null_parses() {
        assert_eq!(
            Expr::parse("shipped_at is not null").unwrap(),
            Expr::IsNull {
                column: "shipped_at".into(),
                negated: true
            }
        );
    }

    #[test]
    fn standalone_terms_parse_for_write_columns() {
        assert_eq!(parse_term(":amount").unwrap(), Term::Param("amount".into()));
        assert_eq!(parse_term("$caller.id").unwrap(), Term::Caller("id".into()));
        assert_eq!(parse_term("now()").unwrap(), Term::Now);
        assert_eq!(parse_term("uuid()").unwrap(), Term::NewUuid);
        assert_eq!(
            parse_term("'manual'").unwrap(),
            Term::Lit(Value::Text("manual".into()))
        );
        assert!(parse_term("order_no").is_err());
    }
}
