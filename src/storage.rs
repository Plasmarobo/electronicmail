//! Compact local store: SQLite with an FTS5 full-text index.
//!
//! `messages` holds canonical rows; `messages_fts` is an external-content-free
//! FTS5 index keyed by `rowid == messages.uid` for fast search.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::model::EmailSummary;
use crate::search::ParsedQuery;
use crate::spam::BayesModel;

pub struct Store {
    conn: Connection,
}

/// Columns selected to build an [`EmailSummary`], in a fixed order shared by
/// every list query so [`row_to_summary`] can decode them positionally.
const SUMMARY_COLS: &str = "uid, account, from_name, from_addr, to_addrs, subject, date_ts, snippet, seen, flagged, \
     spam_score, is_spam, spam_reasons";

/// Everything needed to persist one fetched message.
pub struct StoredMessage {
    /// Per-mailbox IMAP UID (unique only within one account's INBOX).
    pub imap_uid: i64,
    pub account: String,
    pub message_id: String,
    pub from_name: String,
    pub from_addr: String,
    pub to_addrs: String,
    pub subject: String,
    pub date_ts: i64,
    pub snippet: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub seen: bool,
    pub flagged: bool,
    /// Reply-To address (transient — used only for classification, not stored).
    pub reply_to: String,
    /// Raw `Authentication-Results` header (transient — classification only).
    pub auth_results: String,
    /// Spam verdict, filled in by the worker before persisting.
    pub spam_score: f32,
    pub is_spam: bool,
    pub spam_reasons: String,
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// In-memory store (used by tests).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS messages (
                uid         INTEGER PRIMARY KEY,
                account     TEXT NOT NULL,
                imap_uid    INTEGER NOT NULL DEFAULT 0,
                message_id  TEXT,
                from_name   TEXT NOT NULL DEFAULT '',
                from_addr   TEXT NOT NULL DEFAULT '',
                to_addrs    TEXT NOT NULL DEFAULT '',
                subject     TEXT NOT NULL DEFAULT '',
                date_ts     INTEGER NOT NULL DEFAULT 0,
                snippet     TEXT NOT NULL DEFAULT '',
                body_text   TEXT NOT NULL DEFAULT '',
                body_html   TEXT,
                seen        INTEGER NOT NULL DEFAULT 0,
                flagged     INTEGER NOT NULL DEFAULT 0,
                spam_score  REAL NOT NULL DEFAULT 0,
                is_spam     INTEGER NOT NULL DEFAULT 0,
                spam_reasons TEXT NOT NULL DEFAULT ''
            );

            CREATE INDEX IF NOT EXISTS idx_messages_date ON messages(date_ts DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                from_addr,
                from_name,
                to_addrs,
                subject,
                body,
                tokenize = 'unicode61 remove_diacritics 2'
            );

            -- Bayesian spam model (one row of totals + per-token counts).
            CREATE TABLE IF NOT EXISTS spam_meta (
                id            INTEGER PRIMARY KEY CHECK (id = 1),
                ham_messages  INTEGER NOT NULL DEFAULT 0,
                spam_messages INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS spam_tokens (
                token TEXT PRIMARY KEY,
                ham   INTEGER NOT NULL DEFAULT 0,
                spam  INTEGER NOT NULL DEFAULT 0
            );
            "#,
            )
            .context("initializing database schema")?;

        // Migrate older databases that predate the spam columns. Duplicate
        // column errors are expected and ignored.
        for stmt in [
            "ALTER TABLE messages ADD COLUMN spam_score REAL NOT NULL DEFAULT 0",
            "ALTER TABLE messages ADD COLUMN is_spam INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE messages ADD COLUMN spam_reasons TEXT NOT NULL DEFAULT ''",
            // Multi-account: distinguish the per-mailbox IMAP UID from the
            // synthetic primary key so UIDs from different accounts can't collide.
            "ALTER TABLE messages ADD COLUMN imap_uid INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        // For databases created before `imap_uid` existed, the old primary key
        // *was* the IMAP UID, so seed the new column from it.
        let _ = self
            .conn
            .execute("UPDATE messages SET imap_uid = uid WHERE imap_uid = 0", []);
        // One message per (account, IMAP UID). Enables ON CONFLICT upserts.
        self.conn
            .execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_account_uid \
                 ON messages(account, imap_uid)",
                [],
            )
            .context("creating account/uid unique index")?;
        Ok(())
    }

    /// Insert or replace a message and keep the FTS index in sync.
    pub fn upsert(&mut self, m: &StoredMessage) -> Result<()> {
        let tx = self.conn.transaction()?;
        // Upsert on (account, imap_uid). The synthetic `uid` primary key is kept
        // stable across re-fetches (DO UPDATE, not REPLACE) and returned so the
        // FTS row can be re-linked.
        let uid: i64 = tx.query_row(
            r#"INSERT INTO messages
               (account, imap_uid, message_id, from_name, from_addr, to_addrs,
                subject, date_ts, snippet, body_text, body_html, seen, flagged,
                spam_score, is_spam, spam_reasons)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
               ON CONFLICT(account, imap_uid) DO UPDATE SET
                 message_id=excluded.message_id, from_name=excluded.from_name,
                 from_addr=excluded.from_addr, to_addrs=excluded.to_addrs,
                 subject=excluded.subject, date_ts=excluded.date_ts,
                 snippet=excluded.snippet, body_text=excluded.body_text,
                 body_html=excluded.body_html, seen=excluded.seen,
                 flagged=excluded.flagged, spam_score=excluded.spam_score,
                 is_spam=excluded.is_spam, spam_reasons=excluded.spam_reasons
               RETURNING uid"#,
            params![
                m.account,
                m.imap_uid,
                m.message_id,
                m.from_name,
                m.from_addr,
                m.to_addrs,
                m.subject,
                m.date_ts,
                m.snippet,
                m.body_text,
                m.body_html,
                m.seen as i64,
                m.flagged as i64,
                m.spam_score as f64,
                m.is_spam as i64,
                m.spam_reasons,
            ],
            |row| row.get(0),
        )?;
        // FTS rows are not unique-keyed, so delete any prior row first.
        tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![uid])?;
        tx.execute(
            r#"INSERT INTO messages_fts
               (rowid, from_addr, from_name, to_addrs, subject, body)
               VALUES (?1,?2,?3,?4,?5,?6)"#,
            params![
                uid,
                m.from_addr,
                m.from_name,
                m.to_addrs,
                m.subject,
                m.body_text,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The highest IMAP UID currently stored for an account (0 if empty). Used
    /// to fetch only newer messages on subsequent syncs.
    pub fn max_uid(&self, account: &str) -> Result<i64> {
        let uid: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(imap_uid), 0) FROM messages WHERE account = ?1",
            params![account],
            |row| row.get(0),
        )?;
        Ok(uid)
    }

    /// Recent non-spam messages for the inbox view (no query).
    ///
    /// `account == None` combines every account ("All inboxes"). When
    /// `unread_only` is set, already-read messages are hidden.
    pub fn recent(
        &self,
        account: Option<&str>,
        unread_only: bool,
        limit: i64,
    ) -> Result<Vec<EmailSummary>> {
        let mut wheres = vec!["is_spam = 0".to_string()];
        if account.is_some() {
            wheres.push("account = :account".to_string());
        }
        if unread_only {
            wheres.push("seen = 0".to_string());
        }
        let sql = format!(
            "SELECT {SUMMARY_COLS} FROM messages WHERE {} ORDER BY date_ts DESC LIMIT :limit",
            wheres.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
        if let Some(a) = &account {
            binds.push((":account", a));
        }
        binds.push((":limit", &limit));
        let rows = stmt
            .query_map(binds.as_slice(), row_to_summary)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Messages currently filtered as spam, newest first. `account == None`
    /// combines every account.
    pub fn spam(&self, account: Option<&str>, limit: i64) -> Result<Vec<EmailSummary>> {
        let mut wheres = vec!["is_spam = 1".to_string()];
        if account.is_some() {
            wheres.push("account = :account".to_string());
        }
        let sql = format!(
            "SELECT {SUMMARY_COLS} FROM messages WHERE {} ORDER BY date_ts DESC LIMIT :limit",
            wheres.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
        if let Some(a) = &account {
            binds.push((":account", a));
        }
        binds.push((":limit", &limit));
        let rows = stmt
            .query_map(binds.as_slice(), row_to_summary)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Execute a parsed search query. `account == None` searches all accounts.
    pub fn search(
        &self,
        account: Option<&str>,
        query: &ParsedQuery,
        limit: i64,
    ) -> Result<Vec<EmailSummary>> {
        // Build the SQL dynamically depending on which filters are present.
        let mut sql = format!(
            "SELECT {} FROM messages m ",
            SUMMARY_COLS
                .split(", ")
                .map(|c| format!("m.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        // Search never surfaces spam unless the user opens the Spam folder.
        let mut wheres = vec!["m.is_spam = 0".to_string()];
        if account.is_some() {
            wheres.push("m.account = :account".to_string());
        }

        if query.fts_positive.is_some() {
            wheres.push(
                "m.uid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH :pos)"
                    .to_string(),
            );
        }
        if query.fts_negative.is_some() {
            wheres.push(
                "m.uid NOT IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH :neg)"
                    .to_string(),
            );
        }
        if query.after_ts.is_some() {
            wheres.push("m.date_ts >= :after".to_string());
        }
        if query.before_ts.is_some() {
            wheres.push("m.date_ts < :before".to_string());
        }
        if query.unread_only {
            wheres.push("m.seen = 0".to_string());
        }
        if query.read_only {
            wheres.push("m.seen = 1".to_string());
        }
        if query.flagged_only {
            wheres.push("m.flagged = 1".to_string());
        }

        sql.push_str("WHERE ");
        sql.push_str(&wheres.join(" AND "));
        sql.push_str(" ORDER BY m.date_ts DESC LIMIT :limit");

        let mut stmt = self.conn.prepare(&sql)?;

        // Bind only the parameters that appear in the query.
        let mut binds: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
        if let Some(a) = &account {
            binds.push((":account", a));
        }
        if let Some(p) = &query.fts_positive {
            binds.push((":pos", p));
        }
        if let Some(n) = &query.fts_negative {
            binds.push((":neg", n));
        }
        if let Some(a) = &query.after_ts {
            binds.push((":after", a));
        }
        if let Some(b) = &query.before_ts {
            binds.push((":before", b));
        }
        binds.push((":limit", &limit));

        let rows = stmt
            .query_map(binds.as_slice(), row_to_summary)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Load the full body for a single message.
    pub fn body(&self, uid: i64) -> Result<Option<(Option<String>, Option<String>)>> {
        let result = self
            .conn
            .query_row(
                "SELECT body_text, body_html FROM messages WHERE uid = ?1",
                params![uid],
                |row| {
                    let text: String = row.get(0)?;
                    let html: Option<String> = row.get(1)?;
                    Ok((Some(text), html))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(result)
    }

    /// Mark a message as read. Returns its `(account, imap_uid)` so the caller
    /// can mirror the `\Seen` flag on the IMAP server (returns `None` if the
    /// message no longer exists).
    pub fn mark_seen(&self, uid: i64) -> Result<Option<(String, i64)>> {
        self.conn
            .execute("UPDATE messages SET seen = 1 WHERE uid = ?1", params![uid])?;
        let res = self
            .conn
            .query_row(
                "SELECT account, imap_uid FROM messages WHERE uid = ?1",
                params![uid],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(res)
    }

    /// Update the spam verdict for a single message.
    pub fn set_spam(&self, uid: i64, is_spam: bool, score: f32, reasons: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET is_spam = ?1, spam_score = ?2, spam_reasons = ?3 WHERE uid = ?4",
            params![is_spam as i64, score as f64, reasons, uid],
        )?;
        Ok(())
    }

    /// Re-classify every message from a sender across *all* accounts (the
    /// block/allow lists are global, so a block applies everywhere).
    pub fn set_spam_for_sender_all(
        &self,
        from_addr: &str,
        is_spam: bool,
        reasons: &str,
    ) -> Result<()> {
        let score = if is_spam { 1.0 } else { 0.0 };
        self.conn.execute(
            "UPDATE messages SET is_spam = ?1, spam_score = ?2, spam_reasons = ?3 \
             WHERE lower(from_addr) = lower(?4)",
            params![is_spam as i64, score as f64, reasons, from_addr],
        )?;
        Ok(())
    }

    /// The sender address of a message, if known.
    pub fn from_addr(&self, uid: i64) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT from_addr FROM messages WHERE uid = ?1",
                params![uid],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(r)
    }

    /// Subject + sender + body text, used to train the Bayesian model.
    pub fn training_text(&self, uid: i64) -> Result<Option<String>> {
        let r = self
            .conn
            .query_row(
                "SELECT subject, from_addr, body_text FROM messages WHERE uid = ?1",
                params![uid],
                |row| {
                    let subject: String = row.get(0)?;
                    let from: String = row.get(1)?;
                    let body: String = row.get(2)?;
                    Ok(format!("{subject} {from} {body}"))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(r)
    }

    /// Load the persisted Bayesian model (empty if never trained).
    pub fn load_bayes(&self) -> Result<BayesModel> {
        let mut model = BayesModel::default();
        if let Ok((ham, spam)) = self.conn.query_row(
            "SELECT ham_messages, spam_messages FROM spam_meta WHERE id = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ) {
            model.ham_messages = ham.max(0) as u32;
            model.spam_messages = spam.max(0) as u32;
        }
        let mut stmt = self
            .conn
            .prepare("SELECT token, ham, spam FROM spam_tokens")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as u32,
                row.get::<_, i64>(2)? as u32,
            ))
        })?;
        for row in rows {
            let (token, ham, spam) = row?;
            model.tokens.insert(token, (ham, spam));
        }
        Ok(model)
    }

    /// Persist the Bayesian model, replacing the previous snapshot.
    pub fn save_bayes(&mut self, model: &BayesModel) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO spam_meta (id, ham_messages, spam_messages) VALUES (1, ?1, ?2)",
            params![model.ham_messages as i64, model.spam_messages as i64],
        )?;
        tx.execute("DELETE FROM spam_tokens", [])?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO spam_tokens (token, ham, spam) VALUES (?1, ?2, ?3)")?;
            for (token, (ham, spam)) in &model.tokens {
                stmt.execute(params![token, *ham as i64, *spam as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmailSummary> {
    Ok(EmailSummary {
        uid: row.get(0)?,
        account: row.get(1)?,
        from_name: row.get(2)?,
        from_addr: row.get(3)?,
        to_addrs: row.get(4)?,
        subject: row.get(5)?,
        date_ts: row.get(6)?,
        snippet: row.get(7)?,
        seen: row.get::<_, i64>(8)? != 0,
        flagged: row.get::<_, i64>(9)? != 0,
        spam_score: row.get::<_, f64>(10)? as f32,
        is_spam: row.get::<_, i64>(11)? != 0,
        spam_reasons: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search;

    fn sample(uid: i64, from: &str, subject: &str, body: &str, ts: i64) -> StoredMessage {
        StoredMessage {
            imap_uid: uid,
            account: "me@example.com".into(),
            message_id: format!("<{uid}@x>"),
            from_name: "".into(),
            from_addr: from.into(),
            to_addrs: "me@example.com".into(),
            subject: subject.into(),
            date_ts: ts,
            snippet: body.chars().take(40).collect(),
            body_text: body.into(),
            body_html: None,
            seen: false,
            flagged: false,
            reply_to: String::new(),
            auth_results: String::new(),
            spam_score: 0.0,
            is_spam: false,
            spam_reasons: String::new(),
        }
    }

    #[test]
    fn search_roundtrip() {
        let mut s = Store::open_in_memory().unwrap();
        s.upsert(&sample(
            1,
            "alice@a.com",
            "Invoice 42",
            "please pay the invoice",
            100,
        ))
        .unwrap();
        s.upsert(&sample(2, "bob@b.com", "Lunch", "want to grab lunch", 200))
            .unwrap();

        let acc = Some("me@example.com");
        let q = search::parse("invoice");
        let r = s.search(acc, &q, 50).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].from_addr, "alice@a.com");

        let q = search::parse("from:bob");
        let r = s.search(acc, &q, 50).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].from_addr, "bob@b.com");

        let q = search::parse("-invoice");
        let r = s.search(acc, &q, 50).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].from_addr, "bob@b.com");
    }

    #[test]
    fn combined_and_unread_filters() {
        let mut s = Store::open_in_memory().unwrap();
        // Two accounts that happen to share IMAP UID 1 — must not collide.
        let mut a = sample(1, "alice@a.com", "Hi", "from account one", 100);
        a.account = "one@example.com".into();
        let mut b = sample(1, "bob@b.com", "Yo", "from account two", 200);
        b.account = "two@example.com".into();
        let mut c = sample(2, "carol@c.com", "Read", "already read", 50);
        c.account = "one@example.com".into();
        c.seen = true;
        s.upsert(&a).unwrap();
        s.upsert(&b).unwrap();
        s.upsert(&c).unwrap();

        // Combined unread-only across all accounts: a + b (c is read).
        let all = s.recent(None, true, 50).unwrap();
        assert_eq!(all.len(), 2);
        // Per-account, including read: one@ has a + c.
        let one = s.recent(Some("one@example.com"), false, 50).unwrap();
        assert_eq!(one.len(), 2);
        // Per-account, unread only: just a.
        let one_unread = s.recent(Some("one@example.com"), true, 50).unwrap();
        assert_eq!(one_unread.len(), 1);
        assert_eq!(one_unread[0].from_addr, "alice@a.com");
    }
}
