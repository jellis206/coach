//! SQLite evidence store: one writer, WAL, versioned schema, bounded decisions.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::event::EventEnvelope;
use crate::paths::ensure_private_dir;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const RETENTION_MS: i64 = 30 * 86_400_000;
pub const MAX_SUMMARY_BYTES: usize = 4096;
const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Accepted,
    Dismissed,
}

/// A user's decision on a finding. Holds references and a bounded summary,
/// never copies of event payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub finding_id: String,
    pub kind: DecisionKind,
    pub reference: Option<String>,
    pub summary: String,
    pub evidence_ids: Vec<String>,
}

/// Payload-free health counters.
#[derive(Debug, Default, Serialize)]
pub struct Diagnostics {
    pub events: u64,
    pub by_source: BTreeMap<String, u64>,
    pub truncated_events: u64,
    pub redacted_events: u64,
    pub unknown_kind_events: u64,
    pub oldest_received_ms: Option<i64>,
    pub newest_received_ms: Option<i64>,
    pub decisions: u64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let parent = path.parent().ok_or("database path has no parent")?;
        let parent = ensure_private_dir(parent)?;
        let path = parent.join(path.file_name().ok_or("database path has no file name")?);
        // Create the file 0600 before SQLite touches it; WAL/SHM inherit its mode.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        let conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(format!(
                "database schema {version} is newer than this coach ({SCHEMA_VERSION})"
            )
            .into());
        }
        if version < 1 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(
                "CREATE TABLE events (
                    event_id TEXT PRIMARY KEY,
                    received_at_ms INTEGER NOT NULL,
                    occurred_at_ms INTEGER NOT NULL,
                    process_instance_id TEXT NOT NULL,
                    session_id TEXT,
                    agent_id TEXT,
                    parent_agent_id TEXT,
                    turn_id TEXT,
                    tool_call_id TEXT,
                    source TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    envelope TEXT NOT NULL
                );
                CREATE INDEX events_session ON events(session_id, occurred_at_ms);
                CREATE INDEX events_received ON events(received_at_ms);
                CREATE TABLE decisions (
                    finding_id TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    reference TEXT,
                    summary TEXT NOT NULL,
                    evidence_ids TEXT NOT NULL,
                    decided_at_ms INTEGER NOT NULL
                );
                PRAGMA user_version = 1;",
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Sanitize and insert; a repeated `event_id` is ignored.
    pub fn insert(&mut self, event: &EventEnvelope, received_at_ms: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        insert_one(&tx, event, received_at_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// Insert a batch in one transaction (the daemon writer's path).
    pub fn insert_batch(&mut self, events: &[(EventEnvelope, i64)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (event, received_at_ms) in events {
            insert_one(&tx, event, *received_at_ms)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, event_id: &str) -> Result<Option<EventEnvelope>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT envelope FROM events WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    /// Delete detailed evidence received before `before_ms`. Decisions are kept.
    pub fn prune(&mut self, before_ms: i64) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM events WHERE received_at_ms < ?1", [before_ms])?)
    }

    pub fn decide(&mut self, decision: &Decision, decided_at_ms: i64) -> Result<()> {
        let mut summary = decision.summary.clone();
        summary.truncate(summary.floor_char_boundary(MAX_SUMMARY_BYTES));
        self.conn.execute(
            "INSERT INTO decisions (finding_id, kind, reference, summary, evidence_ids, decided_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(finding_id) DO UPDATE SET kind = excluded.kind, reference = excluded.reference,
               summary = excluded.summary, evidence_ids = excluded.evidence_ids, decided_at_ms = excluded.decided_at_ms",
            params![
                decision.finding_id,
                serde_json::to_value(decision.kind)?.as_str(),
                decision.reference,
                summary,
                serde_json::to_string(&decision.evidence_ids)?,
                decided_at_ms
            ],
        )?;
        Ok(())
    }

    pub fn decision(&self, finding_id: &str) -> Result<Option<Decision>> {
        let row = self
            .conn
            .query_row(
                "SELECT kind, reference, summary, evidence_ids FROM decisions WHERE finding_id = ?1",
                [finding_id],
                |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)),
            )
            .optional()?;
        let Some((kind, reference, summary, ids)) = row else {
            return Ok(None);
        };
        Ok(Some(Decision {
            finding_id: finding_id.into(),
            kind: serde_json::from_value(serde_json::Value::String(kind))?,
            reference,
            summary,
            evidence_ids: serde_json::from_str(&ids)?,
        }))
    }

    pub fn diagnostics(&self) -> Result<Diagnostics> {
        let mut d = Diagnostics::default();
        let mut stmt = self
            .conn
            .prepare("SELECT source, COUNT(*) FROM events GROUP BY source")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (source, n) = row?;
            let n = n as u64;
            d.events += n;
            d.by_source.insert(source, n);
        }
        (d.oldest_received_ms, d.newest_received_ms) = self.conn.query_row(
            "SELECT MIN(received_at_ms), MAX(received_at_ms) FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let count = |sql: &str| -> Result<u64> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
        };
        let status = "json_extract(envelope, '$.capture_status')";
        d.truncated_events = count(&format!(
            "SELECT COUNT(*) FROM events WHERE json_array_length({status}, '$.truncated') > 0"
        ))?;
        d.redacted_events = count(&format!(
            "SELECT COUNT(*) FROM events WHERE json_array_length({status}, '$.redacted') > 0"
        ))?;
        d.unknown_kind_events = count(&format!(
            "SELECT COUNT(*) FROM events WHERE json_extract({status}, '$.unknown_kind')"
        ))?;
        d.decisions = count("SELECT COUNT(*) FROM decisions")?;
        Ok(d)
    }

    pub fn journal_mode(&self) -> Result<String> {
        Ok(self
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))?)
    }

    pub fn user_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }
}

fn insert_one(conn: &Connection, event: &EventEnvelope, received_at_ms: i64) -> Result<()> {
    let mut event = event.clone();
    event.sanitize();
    conn.execute(
        "INSERT OR IGNORE INTO events (event_id, received_at_ms, occurred_at_ms, process_instance_id, session_id,
           agent_id, parent_agent_id, turn_id, tool_call_id, source, kind, envelope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            event.event_id,
            received_at_ms,
            event.occurred_at_ms,
            event.process_instance_id,
            event.session_id,
            event.agent_id,
            event.parent_agent_id,
            event.turn_id,
            event.tool_call_id,
            event.source,
            event.kind,
            serde_json::to_string(&event)?
        ],
    )?;
    Ok(())
}
