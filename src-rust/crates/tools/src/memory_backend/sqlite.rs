//! The sqlite-backed memory engine: an FTS5 database instead of `.md` files.
//!
//! One `memory.db` inside the auto-memory directory holds every memory as a row
//! in a `memories` table, mirrored into an FTS5 index for search. The `kind`
//! column (`lesson`, `fact`, `session_note`, `topic`) keeps the four sources
//! apart. On first open the existing `.md` files are imported once, so
//! switching a project to sqlite does not lose what it already knew;
//! [`export_to_files`] does the reverse when a project switches back.
//!
//! rusqlite is blocking, so the async trait methods do their work inline. The
//! store is small (bounded per kind), so a query is a handful of rows.

use super::{MemoryBackend, MemoryHit};
use crate::ToolResult;
use async_trait::async_trait;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Entries kept per kind; the oldest drops when a new one arrives, matching the
/// file engine's cap.
const CAP_PER_KIND: usize = 100;

/// Schema: one content table, one FTS5 index, and triggers that keep them in
/// sync. `bm25(8,4,1)` weights title over description over body, reproducing
/// the file engine's name/description/body ranking.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    title TEXT,
    description TEXT,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    norm_key TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS memories_kind_key ON memories(kind, norm_key);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    title, description, body, content='memories', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, description, body)
        VALUES (new.id, new.title, new.description, new.body);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, description, body)
        VALUES ('delete', old.id, old.title, old.description, old.body);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, description, body)
        VALUES ('delete', old.id, old.title, old.description, old.body);
    INSERT INTO memories_fts(rowid, title, description, body)
        VALUES (new.id, new.title, new.description, new.body);
END;";

pub struct SqliteBackend {
    memory_dir: PathBuf,
}

impl SqliteBackend {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }

    fn db_path(&self) -> PathBuf {
        self.memory_dir.join("memory.db")
    }

    /// Open the database, creating the schema, and import the existing `.md`
    /// files the first time the file does not exist yet.
    fn open(&self) -> rusqlite::Result<Connection> {
        let path = self.db_path();
        let fresh = !path.exists();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(SCHEMA)?;
        if fresh {
            self.import_markdown(&conn);
        }
        Ok(conn)
    }

    /// One-time import of the `.md` store into the database, on first open.
    /// Best effort: a file that cannot be read is skipped, not fatal.
    fn import_markdown(&self, conn: &Connection) {
        let entries = match std::fs::read_dir(&self.memory_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            self.import_one(conn, kind_for_filename(filename), filename, &content);
        }
    }

    /// Import one file. `learned.md` and `facts.md` import per entry, so each
    /// lesson or fact is its own searchable row and survives the reverse export
    /// unchanged; every other file imports whole, as one row of its kind.
    fn import_one(&self, conn: &Connection, kind: &str, filename: &str, content: &str) {
        if matches!(kind, "lesson" | "fact") {
            for item in entry_items(content) {
                let _ = self.insert_row(conn, kind, kind, None, &item);
            }
            return;
        }
        let (name, description, _) = mikmik_core::memdir::parse_frontmatter_quick(content);
        let title = name.unwrap_or_else(|| filename.to_string());
        let _ = self.insert_row(conn, kind, &title, description.as_deref(), content);
    }

    /// Insert or refresh one row, enforcing the per-kind cap. Returns whether a
    /// new row was written (`true`) or a duplicate was found (`false`).
    fn insert_row(
        &self,
        conn: &Connection,
        kind: &str,
        title: &str,
        description: Option<&str>,
        body: &str,
    ) -> rusqlite::Result<bool> {
        let norm = normalise(body);
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM memories WHERE kind = ?1 AND norm_key = ?2",
                params![kind, norm],
                |row| row.get(0),
            )
            .ok();
        if existing.is_some() {
            return Ok(false);
        }
        let now = now_secs() as i64;
        conn.execute(
            "INSERT INTO memories (kind, title, description, body, created_at, updated_at, norm_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![kind, title, description, body, now, norm],
        )?;
        self.enforce_cap(conn, kind)?;
        Ok(true)
    }

    /// Drop the oldest rows of a kind beyond the cap.
    fn enforce_cap(&self, conn: &Connection, kind: &str) -> rusqlite::Result<()> {
        conn.execute(
            "DELETE FROM memories WHERE kind = ?1 AND id NOT IN (
                 SELECT id FROM memories WHERE kind = ?1 ORDER BY updated_at DESC LIMIT ?2
             )",
            params![kind, CAP_PER_KIND as i64],
        )?;
        Ok(())
    }

    /// The write path shared by `append_lesson` and `retain_fact`.
    fn record(&self, kind: &str, noun: &str, item: &str, topic: Option<&str>) -> ToolResult {
        let redacted = mikmik_core::redact::redact_secrets(item);
        let title = topic
            .map(str::trim)
            .filter(|topic| !topic.is_empty())
            .unwrap_or(noun)
            .to_string();
        let conn = match self.open() {
            Ok(conn) => conn,
            Err(error) => {
                return ToolResult::error(format!("Could not open memory database: {error}"))
            }
        };
        match self.insert_row(&conn, kind, &title, None, &redacted.text) {
            Ok(true) => ToolResult::success(build_record_report(noun, &redacted.classes)),
            Ok(false) => ToolResult::success(format!(
                "Already recorded, so nothing was written. The memory database holds this {noun}."
            )),
            Err(error) => ToolResult::error(format!("Could not write the {noun}: {error}")),
        }
    }

    /// Persist auto-extracted session notes as `session_note` rows, one per
    /// note, credentials redacted. The sqlite counterpart of
    /// `SessionMemoryExtractor::persist`: on the sqlite engine the notes become
    /// searchable rows instead of a `session-notes.md` section. Each note's
    /// category labels the row for search ranking. Best effort: a database that
    /// cannot open drops the notes rather than failing the turn.
    pub fn persist_session_notes(&self, notes: &[(String, String)]) {
        let conn = match self.open() {
            Ok(conn) => conn,
            Err(error) => {
                tracing::debug!(error = %error, "sqlite session-note persist skipped");
                return;
            }
        };
        for (label, content) in notes {
            let redacted = mikmik_core::redact::redact_secrets(content);
            let title = format!("session note: {label}");
            let _ = self.insert_row(&conn, "session_note", &title, None, &redacted.text);
        }
    }

    fn search_impl(&self, query: &str, max_files: usize) -> rusqlite::Result<Vec<MemoryHit>> {
        let match_query = build_fts_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.open()?;
        let mut stmt = conn.prepare(
            "SELECT m.title, m.body, m.updated_at, m.kind FROM memories_fts f
             JOIN memories m ON m.id = f.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY bm25(memories_fts, 8.0, 4.0, 1.0)
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![match_query, max_files as i64], |row| {
            let title: Option<String> = row.get(0)?;
            let body: String = row.get(1)?;
            let updated: i64 = row.get(2)?;
            let kind: String = row.get(3)?;
            Ok(MemoryHit {
                title: title.unwrap_or(kind),
                body,
                modified_secs: updated.max(0) as u64,
            })
        })?;
        rows.collect()
    }

    /// The compact index for the system prompt: a count for each append-style
    /// kind that has rows, then one line per topic file. Per-entry lessons and
    /// facts would flood the prompt if listed one by one, so they are summed.
    fn index_lines(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.open()?;
        let mut lines = Vec::new();
        for (kind, plural) in [
            ("lesson", "lessons"),
            ("fact", "facts"),
            ("session_note", "session notes"),
        ] {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memories WHERE kind = ?1",
                params![kind],
                |row| row.get(0),
            )?;
            if count > 0 {
                lines.push(format!("- {count} {plural}"));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT title, description FROM memories WHERE kind = 'topic' ORDER BY updated_at DESC",
        )?;
        let topics = stmt.query_map([], |row| {
            let title: Option<String> = row.get(0)?;
            let description: Option<String> = row.get(1)?;
            Ok((title.unwrap_or_default(), description))
        })?;
        for (title, description) in topics.flatten() {
            match description {
                Some(desc) if !desc.is_empty() => lines.push(format!("- {title}: {desc}")),
                _ => lines.push(format!("- {title}")),
            }
        }
        Ok(lines)
    }
}

#[async_trait]
impl MemoryBackend for SqliteBackend {
    fn prompt_block(&self) -> String {
        let lines = match self.index_lines() {
            Ok(lines) => lines,
            Err(error) => {
                tracing::debug!(error = %error, "sqlite memory manifest failed");
                return String::new();
            }
        };
        if lines.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "Memory database: {}\nFor one durable lesson, call the `Learn` tool; for a durable \
             fact, call `Retain`. Search recall with the `Memory` tool. Both file and \
             deduplicate for you.\n\n## Memory Index",
            self.db_path().display()
        );
        for line in lines {
            out.push('\n');
            out.push_str(&line);
        }
        out
    }

    fn search(&self, query: &str, max_files: usize) -> Vec<MemoryHit> {
        match self.search_impl(query, max_files) {
            Ok(hits) => hits,
            Err(error) => {
                tracing::debug!(error = %error, "sqlite memory search failed");
                Vec::new()
            }
        }
    }

    async fn append_lesson(
        &self,
        item: &str,
        topic: Option<&str>,
        _context: Option<&str>,
    ) -> ToolResult {
        self.record("lesson", "lesson", item, topic)
    }

    async fn retain_fact(
        &self,
        item: &str,
        topic: Option<&str>,
        _context: Option<&str>,
    ) -> ToolResult {
        self.record("fact", "fact", item, topic)
    }
}

/// Which `kind` an imported filename maps to.
fn kind_for_filename(filename: &str) -> &'static str {
    match filename {
        "learned.md" => "lesson",
        "facts.md" => "fact",
        "session-notes.md" => "session_note",
        _ => "topic",
    }
}

/// The item sentence of every entry in an append-style `.md` body: the first
/// non-empty, non-context line under each `## ` heading. Used by per-entry
/// import so lessons and facts land as one row each.
fn entry_items(content: &str) -> Vec<String> {
    crate::memory_append::parse_entries(crate::memory_append::body_after_frontmatter(content))
        .into_iter()
        .filter_map(|entry| first_item_line(&entry.text))
        .collect()
}

/// The line under an entry heading that carries the item, skipping the heading
/// and any `_context:` line.
fn first_item_line(block: &str) -> Option<String> {
    block
        .lines()
        .skip(1)
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("_context:"))
        .map(str::to_string)
}

/// Export the sqlite store back to `.md` files when a project switches to the
/// file engine, then set the database aside so the export runs once. Only the
/// append-style stores are exported; session notes and topic files were
/// imported from `.md` that still exist untouched, so re-writing them would
/// duplicate what is already on disk.
pub(crate) fn export_to_files(memory_dir: &Path) {
    let db = memory_dir.join("memory.db");
    if !db.exists() {
        return;
    }
    let conn = match Connection::open(&db) {
        Ok(conn) => conn,
        Err(_) => return,
    };
    let lessons = read_items(&conn, "lesson");
    let facts = read_items(&conn, "fact");
    let _ = crate::memory_append::merge_entries(memory_dir, &super::file::LEARN, &lessons);
    let _ = crate::memory_append::merge_entries(memory_dir, &super::file::RETAIN, &facts);
    drop(conn);
    let _ = std::fs::rename(&db, memory_dir.join("memory.db.bak"));
}

/// Read the item text of every row of a kind, oldest first, so the newest lands
/// on top after each is prepended during the merge.
fn read_items(conn: &Connection, kind: &str) -> Vec<String> {
    let mut stmt =
        match conn.prepare("SELECT body FROM memories WHERE kind = ?1 ORDER BY updated_at ASC") {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
    let items = match stmt.query_map(params![kind], |row| row.get::<_, String>(0)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    };
    items
}

/// Lower-case, whitespace collapsed, for the dedup key.
fn normalise(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote each term and OR them, so punctuation in a query cannot break the
/// FTS5 MATCH grammar and any term matching is enough to surface a row.
fn build_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "")))
        .filter(|term| term.len() > 2)
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn build_record_report(noun: &str, masked: &[&'static str]) -> String {
    let mut report = format!("Recorded a {noun} in the memory database.");
    if !masked.is_empty() {
        report.push_str(&format!(
            " A credential was masked before writing ({}).",
            masked.join(", ")
        ));
    }
    report
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(dir: &std::path::Path) -> SqliteBackend {
        SqliteBackend::new(dir.join("memory"))
    }

    #[tokio::test]
    async fn a_lesson_is_written_and_found_by_search() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let be = backend(tmp.path());

        let result = be
            .append_lesson("Cargo commands run from src-rust", Some("build"), None)
            .await;
        assert!(!result.is_error, "{}", result.content);

        let hits = be.search("cargo", 5);
        assert_eq!(hits.len(), 1, "the lesson was not found");
        assert!(hits[0].body.contains("src-rust"));
    }

    #[tokio::test]
    async fn a_fact_and_a_lesson_do_not_collide() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let be = backend(tmp.path());
        be.append_lesson("shared text here", None, None).await;
        be.retain_fact("shared text here", None, None).await;
        // Same text, different kinds: both stored.
        let hits = be.search("shared", 5);
        assert_eq!(hits.len(), 2, "kinds must not dedup against each other");
    }

    #[tokio::test]
    async fn the_same_lesson_is_not_written_twice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let be = backend(tmp.path());
        be.append_lesson("one lesson", None, None).await;
        let again = be.append_lesson("  ONE   lesson ", None, None).await;
        assert!(
            again.content.contains("Already recorded"),
            "{}",
            again.content
        );
        assert_eq!(be.search("lesson", 5).len(), 1);
    }

    #[tokio::test]
    async fn a_credential_is_masked_before_writing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let be = backend(tmp.path());
        let secret = format!("ghp{}{}", "_", "A".repeat(30));
        let result = be
            .append_lesson(&format!("token is {secret}"), None, None)
            .await;
        assert!(result.content.contains("masked"), "{}", result.content);
        let hits = be.search("token", 5);
        assert!(!hits[0].body.contains(&secret));
    }

    #[test]
    fn existing_markdown_is_imported_on_first_open() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(&mem).expect("mkdir");
        std::fs::write(
            mem.join("learned.md"),
            "---\nname: Learned lessons\ndescription: d\ntype: project\n---\n\n## 2026-01-01\nthe relay binds 8350",
        )
        .expect("write");
        let be = SqliteBackend::new(mem);
        // First search opens the db, which triggers the import.
        let hits = be.search("relay", 5);
        assert_eq!(hits.len(), 1, "the markdown file was not imported");
    }

    #[test]
    fn a_learned_file_with_two_entries_imports_as_two_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(&mem).expect("mkdir");
        std::fs::write(
            mem.join("learned.md"),
            "---\nname: Learned lessons\ndescription: d\ntype: project\n---\n\n\
             ## 2026-01-01\ncargo runs from src-rust\n\n## 2026-01-02\nthe relay binds 8350\n",
        )
        .expect("write");
        let be = SqliteBackend::new(mem);

        let cargo = be.search("cargo", 5);
        assert_eq!(cargo.len(), 1, "the first entry was not imported");
        assert!(cargo[0].body.contains("cargo runs from src-rust"));
        assert!(
            !cargo[0].body.contains("Learned lessons"),
            "frontmatter leaked into the row body: {}",
            cargo[0].body
        );
        assert_eq!(be.search("relay", 5).len(), 1, "the second entry was lost");
    }

    #[tokio::test]
    async fn switching_back_to_files_exports_the_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = tmp.path().join("memory");
        let be = SqliteBackend::new(mem.clone());
        be.append_lesson("cargo runs from src-rust", None, None)
            .await;
        be.retain_fact("the relay binds 8350", None, None).await;

        export_to_files(&mem);

        let learned = std::fs::read_to_string(mem.join("learned.md")).expect("learned.md");
        assert!(learned.contains("cargo runs from src-rust"), "{learned}");
        let facts = std::fs::read_to_string(mem.join("facts.md")).expect("facts.md");
        assert!(facts.contains("the relay binds 8350"), "{facts}");
        assert!(
            !mem.join("memory.db").exists(),
            "the database was not set aside"
        );
        assert!(mem.join("memory.db.bak").exists(), "the backup is missing");
    }

    #[test]
    fn session_notes_persist_as_searchable_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let be = backend(tmp.path());
        be.persist_session_notes(&[
            (
                "decision".to_string(),
                "we chose sqlite for memory".to_string(),
            ),
            (
                "constraint".to_string(),
                "cargo runs from src-rust".to_string(),
            ),
        ]);
        assert_eq!(be.search("sqlite", 5).len(), 1, "the first note was lost");
        let hits = be.search("cargo", 5);
        assert_eq!(hits.len(), 1, "the second note was lost");
        assert!(hits[0].body.contains("src-rust"));
    }

    #[tokio::test]
    async fn export_does_not_duplicate_entries_already_in_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(&mem).expect("mkdir");
        std::fs::write(
            mem.join("learned.md"),
            "---\nname: Learned lessons\ndescription: d\ntype: project\n---\n\n\
             ## 2026-01-01\ncargo runs from src-rust\n",
        )
        .expect("write");
        // Opening imports the file into the database.
        let be = SqliteBackend::new(mem.clone());
        let _ = be.search("cargo", 5);

        // Switching back exports; the entry is already on disk, so it stays once.
        export_to_files(&mem);

        let learned = std::fs::read_to_string(mem.join("learned.md")).expect("read");
        assert_eq!(
            learned.matches("cargo runs from src-rust").count(),
            1,
            "the export duplicated an entry:\n{learned}"
        );
    }
}
