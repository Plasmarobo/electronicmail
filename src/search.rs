//! Intuitive + advanced search.
//!
//! Translates a human query into an SQLite FTS5 `MATCH` expression plus a set
//! of structured filters (date ranges, read/flag state) applied in SQL.
//!
//! Supported syntax:
//!   - bare words        → full-text AND match
//!   - "quoted phrase"   → exact phrase
//!   - +word             → required term (same as bare; provided for parity)
//!   - -word             → exclude term
//!   - from:alice        → match sender name/address
//!   - to:bob            → match recipients
//!   - subject:invoice   → match subject only
//!   - body:contract     → match body only
//!   - after:2024-01-31  → received on/after date
//!   - before:2024-12-01 → received before date
//!   - is:unread|read|flagged
//!   - has:attachment    (reserved; currently a no-op placeholder)

use chrono::NaiveDate;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    /// FTS5 MATCH expression for required terms, or `None` when absent.
    pub fts_positive: Option<String>,
    /// FTS5 MATCH expression for excluded terms (applied via `NOT IN`).
    pub fts_negative: Option<String>,
    pub after_ts: Option<i64>,
    pub before_ts: Option<i64>,
    pub unread_only: bool,
    pub read_only: bool,
    pub flagged_only: bool,
}

/// Map a user-facing field name to the FTS column(s) it searches.
fn field_columns(field: &str) -> Option<&'static str> {
    match field {
        "from" => Some("{from_addr from_name}"),
        "to" => Some("to_addrs"),
        "subject" | "subj" => Some("subject"),
        "body" => Some("body"),
        _ => None,
    }
}

/// Split a query into tokens, keeping `"quoted phrases"` (and `field:"phrase"`)
/// together as a single token.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Escape a bare term into a safe FTS5 string token (always quoted) so that
/// special characters never break the MATCH expression.
fn fts_quote(term: &str) -> String {
    // Strip surrounding quotes if the user already supplied a phrase, then
    // re-quote uniformly, doubling any embedded quotes per FTS5 rules.
    let inner = term.trim_matches('"');
    let escaped = inner.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn parse_date(value: &str) -> Option<i64> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

pub fn parse(input: &str) -> ParsedQuery {
    let mut q = ParsedQuery::default();
    let mut positives: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();

    for raw in tokenize(input) {
        let mut token = raw.as_str();
        let mut negate = false;
        if let Some(rest) = token.strip_prefix('-') {
            negate = true;
            token = rest;
        } else if let Some(rest) = token.strip_prefix('+') {
            token = rest;
        }
        if token.is_empty() {
            continue;
        }

        // Split `field:value`, but only when the prefix is a known field and the
        // colon is not inside a quoted phrase.
        if let Some((field, value)) = split_field(token) {
            match field {
                "after" | "since" => {
                    if let Some(ts) = parse_date(value) {
                        q.after_ts = Some(ts);
                    }
                    continue;
                }
                "before" | "until" => {
                    if let Some(ts) = parse_date(value) {
                        q.before_ts = Some(ts);
                    }
                    continue;
                }
                "is" => {
                    match value {
                        "unread" | "unseen" => q.unread_only = true,
                        "read" | "seen" => q.read_only = true,
                        "flagged" | "starred" => q.flagged_only = true,
                        _ => {}
                    }
                    continue;
                }
                "has" => {
                    // Placeholder for attachment filtering; ignored for now.
                    continue;
                }
                _ => {
                    if let Some(cols) = field_columns(field) {
                        let expr = format!("{cols} : {}", fts_quote(value));
                        if negate {
                            negatives.push(expr);
                        } else {
                            positives.push(expr);
                        }
                        continue;
                    }
                    // Unknown field → treat the whole token as a plain term.
                }
            }
        }

        let expr = fts_quote(token);
        if negate {
            negatives.push(expr);
        } else {
            positives.push(expr);
        }
    }

    let mut clauses: Vec<String> = Vec::new();
    if !positives.is_empty() {
        clauses.push(positives.join(" AND "));
    }
    if !clauses.is_empty() {
        q.fts_positive = Some(clauses.join(" "));
    }
    if !negatives.is_empty() {
        // Exclude messages matching ANY negative term.
        q.fts_negative = Some(negatives.join(" OR "));
    }

    q
}

/// Returns `Some((field, value))` if `token` looks like `field:value` where the
/// colon precedes the first quote (so `"a:b"` stays a phrase).
fn split_field(token: &str) -> Option<(&str, &str)> {
    let colon = token.find(':')?;
    let quote = token.find('"');
    if let Some(qpos) = quote {
        if qpos < colon {
            return None;
        }
    }
    let field = &token[..colon];
    let value = &token[colon + 1..];
    if field.is_empty() || value.is_empty() {
        return None;
    }
    Some((field, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_terms_are_anded() {
        let q = parse("hello world");
        assert_eq!(q.fts_positive.as_deref(), Some("\"hello\" AND \"world\""));
        assert_eq!(q.fts_negative, None);
    }

    #[test]
    fn negation_and_phrase() {
        let q = parse("\"quarterly report\" -draft");
        assert_eq!(q.fts_positive.as_deref(), Some("\"quarterly report\""));
        assert_eq!(q.fts_negative.as_deref(), Some("\"draft\""));
    }

    #[test]
    fn field_filters() {
        let q = parse("from:alice subject:invoice");
        assert_eq!(
            q.fts_positive.as_deref(),
            Some("{from_addr from_name} : \"alice\" AND subject : \"invoice\"")
        );
    }

    #[test]
    fn structured_filters() {
        let q = parse("is:unread after:2024-01-01 budget");
        assert!(q.unread_only);
        assert_eq!(q.after_ts, parse_date("2024-01-01"));
        assert_eq!(q.fts_positive.as_deref(), Some("\"budget\""));
    }
}
