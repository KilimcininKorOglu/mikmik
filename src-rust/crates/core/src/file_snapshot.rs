//! What the model has actually read from each file, and how strictly an edit
//! is held to it.
//!
//! The editing tools address a file by content: `old_string` is both the
//! address and its own proof, so a wrong address fails to match. That proof
//! covers one thing only. It says the text is somewhere in the file *now*. It
//! says nothing about whether the model ever saw that text, and nothing about
//! whether the file is still the file the model reasoned about.
//!
//! This module supplies the missing half. [`FileReadTool`] and [`FileWriteTool`]
//! record what they observed; the editing tools compare the live file with that
//! record before they write.
//!
//! Two independent facts are kept per path:
//!
//! * the content hash, which answers "is this still the file that was read";
//! * the displayed line numbers, which answer "did the model ever see the lines
//!   it is about to change".
//!
//! Both guards stay silent for a path with no record, because a session that
//! never read a file has nothing to be held to. Enforcing read-before-edit is a
//! separate policy and this module does not implement it.
//!
//! [`FileReadTool`]: https://docs.rs/  <!-- crate-local type, see mikmik-tools -->

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// How strictly an edit is held to what the session has read.
///
/// A ladder, not a set of independent switches: the seen-line check is only
/// meaningful once the content is known to be unchanged, because a line number
/// from a stale read points at different text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditGuard {
    /// No check. The behaviour this tree had before the guard existed.
    #[default]
    Off,
    /// Refuse an edit to a file that changed after the session read it. Also
    /// refuses an edit that writes back the bytes already on disk, and says
    /// more the third time one `old_string` fails against one file.
    Stale,
    /// Everything `stale` refuses, and also an edit whose target lines the
    /// session never displayed.
    Strict,
}

impl EditGuard {
    /// The value written in `settings.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            EditGuard::Off => "off",
            EditGuard::Stale => "stale",
            EditGuard::Strict => "strict",
        }
    }

    /// Read a configured value.
    ///
    /// An unreadable value reads as [`EditGuard::Off`] rather than failing the
    /// session, because a typo in one settings key must not stop the agent from
    /// editing anything at all.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "stale" => EditGuard::Stale,
            "strict" => EditGuard::Strict,
            _ => EditGuard::Off,
        }
    }

    /// Every value, in ladder order, for a settings row and a usage message.
    pub const ALL: [&'static str; 3] = ["off", "stale", "strict"];

    /// Whether this level compares the live file with what was read.
    pub fn checks_staleness(self) -> bool {
        !matches!(self, EditGuard::Off)
    }

    /// Whether this level requires the edited lines to have been displayed.
    pub fn checks_seen_lines(self) -> bool {
        matches!(self, EditGuard::Strict)
    }
}

/// One observation of a file's content.
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    /// Hash of the normalized content, from [`hash_content`].
    pub hash: String,
    /// 1-indexed lines a producer actually displayed.
    ///
    /// `None` means no provenance was recorded, and the seen-line check then
    /// passes. A tool that wrote the file itself records `None`: the content
    /// came from the model, so there is nothing it has not seen.
    pub seen_lines: Option<BTreeSet<usize>>,
}

impl FileSnapshot {
    /// Whether every line in `range` was displayed.
    ///
    /// True when no provenance was recorded, because an unrecorded read is not
    /// evidence of a blind edit.
    pub fn covers(&self, range: std::ops::RangeInclusive<usize>) -> bool {
        match &self.seen_lines {
            None => true,
            Some(seen) => range.into_iter().all(|line| seen.contains(&line)),
        }
    }

    /// The lines of `range` that were never displayed, in order.
    pub fn unseen_in(&self, range: std::ops::RangeInclusive<usize>) -> Vec<usize> {
        match &self.seen_lines {
            None => Vec::new(),
            Some(seen) => range.into_iter().filter(|l| !seen.contains(l)).collect(),
        }
    }
}

/// Strip a byte-order mark and normalize every line ending to `\n`.
///
/// Hashing the normalized form means a file whose line endings changed but
/// whose text did not still counts as the file that was read, which matches how
/// the editing tools match `old_string`.
pub fn normalize_for_hash(text: &str) -> String {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    if body.contains('\r') {
        body.replace("\r\n", "\n")
    } else {
        body.to_string()
    }
}

/// Content hash of a file, as lowercase hex.
///
/// SHA-256 because the crate is already a dependency and its output is stable
/// across builds. The strength is beside the point; determinism is not.
pub fn hash_content(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_for_hash(text).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// How many paths one session keeps a record for.
///
/// A wide session touches far more than a handful of files, and forgetting one
/// only turns a guard off for that path, so the ceiling is generous.
const MAX_PATHS: usize = 512;

/// How many distinct failures one session remembers, for the repeat counter.
const MAX_FAILURE_KEYS: usize = 256;

/// Per-session record of what was read, and of what keeps failing.
///
/// Bounded and in memory only. Nothing here is persisted: a resumed session has
/// read nothing yet, and every guard correctly stays silent until it does.
#[derive(Debug, Default)]
pub struct FileSnapshotStore {
    entries: HashMap<PathBuf, FileSnapshot>,
    /// Insertion order, oldest first, for eviction.
    order: VecDeque<PathBuf>,
    /// How many times one `old_string` has failed against one path.
    failures: HashMap<(PathBuf, String), u32>,
    failure_order: VecDeque<(PathBuf, String)>,
}

impl FileSnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the content a producer observed.
    ///
    /// `seen` are the 1-indexed lines it displayed, or `None` when it displayed
    /// nothing the model has not already got in full. Recording the same
    /// content again widens the seen set instead of replacing it, so two
    /// partial reads of one file add up. Recording different content replaces
    /// the record, because the earlier line numbers no longer mean anything.
    pub fn record(&mut self, path: &Path, content: &str, seen: Option<BTreeSet<usize>>) {
        let hash = hash_content(content);
        let key = path.to_path_buf();

        if let Some(existing) = self.entries.get_mut(&key) {
            if existing.hash == hash {
                match (&mut existing.seen_lines, seen) {
                    // Either side claiming full coverage wins: a whole-file read
                    // after a range read leaves nothing unseen.
                    (None, _) => {}
                    (slot @ Some(_), None) => *slot = None,
                    (Some(have), Some(more)) => have.extend(more),
                }
                return;
            }
        }

        self.entries.insert(
            key.clone(),
            FileSnapshot {
                hash,
                seen_lines: seen,
            },
        );
        self.order.retain(|p| p != &key);
        self.order.push_back(key);
        while self.order.len() > MAX_PATHS {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    /// What was last observed for `path`, if anything.
    pub fn snapshot(&self, path: &Path) -> Option<&FileSnapshot> {
        self.entries.get(path)
    }

    /// Drop the record for `path`.
    ///
    /// Used when a tool changed a file in a way it cannot map back onto the
    /// recorded line numbers. Forgetting turns both guards off for that path
    /// until it is read again, which is the honest outcome: nothing is known
    /// about it any more.
    pub fn forget(&mut self, path: &Path) {
        self.entries.remove(path);
        self.order.retain(|p| p != path);
    }

    /// Count one failed match, and report how many times it has failed.
    ///
    /// Keyed by the text that failed rather than the whole call, so the counter
    /// tracks one stuck attempt and a genuinely different edit starts at one.
    pub fn note_failure(&mut self, path: &Path, old_string: &str) -> u32 {
        let key = (path.to_path_buf(), hash_content(old_string));
        let count = self.failures.entry(key.clone()).or_insert(0);
        *count += 1;
        let count = *count;

        self.failure_order.retain(|k| k != &key);
        self.failure_order.push_back(key);
        while self.failure_order.len() > MAX_FAILURE_KEYS {
            if let Some(oldest) = self.failure_order.pop_front() {
                self.failures.remove(&oldest);
            }
        }
        count
    }

    /// Forget every failure recorded against `path`.
    ///
    /// Called after a successful edit: the file moved, so a previous failure
    /// says nothing about the next attempt.
    pub fn clear_failures(&mut self, path: &Path) {
        self.failures.retain(|(p, _), _| p != path);
        self.failure_order.retain(|(p, _)| p != path);
    }
}

/// How many times one `old_string` may fail against one file before the error
/// stops repeating itself and tells the model to read instead.
pub const REPEATED_FAILURE_LIMIT: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(range: std::ops::RangeInclusive<usize>) -> BTreeSet<usize> {
        range.collect()
    }

    #[test]
    fn an_unreadable_guard_value_turns_the_guard_off() {
        // A typo in one settings key must never stop the agent editing at all.
        assert_eq!(EditGuard::parse("STRICT"), EditGuard::Strict);
        assert_eq!(EditGuard::parse(" stale "), EditGuard::Stale);
        assert_eq!(EditGuard::parse("aggressive"), EditGuard::Off);
        assert_eq!(EditGuard::default(), EditGuard::Off);
    }

    #[test]
    fn each_level_checks_what_its_name_says() {
        assert!(!EditGuard::Off.checks_staleness());
        assert!(!EditGuard::Off.checks_seen_lines());
        assert!(EditGuard::Stale.checks_staleness());
        assert!(!EditGuard::Stale.checks_seen_lines());
        assert!(EditGuard::Strict.checks_staleness());
        assert!(EditGuard::Strict.checks_seen_lines());
    }

    /// The editing tools match `old_string` against an LF-normalized view, so a
    /// file whose endings changed still matches. The hash has to agree, or a
    /// pure line-ending rewrite would read as "somebody replaced this file".
    #[test]
    fn a_line_ending_change_is_not_a_content_change() {
        assert_eq!(hash_content("a\r\nb\r\n"), hash_content("a\nb\n"));
        assert_eq!(hash_content("\u{feff}a\n"), hash_content("a\n"));
        assert_ne!(hash_content("a\n"), hash_content("A\n"));
    }

    #[test]
    fn two_partial_reads_of_one_file_add_up() {
        let mut store = FileSnapshotStore::new();
        let path = Path::new("/tmp/a.rs");
        store.record(path, "x\n", Some(lines(1..=10)));
        store.record(path, "x\n", Some(lines(40..=50)));

        let snapshot = store.snapshot(path).expect("recorded");
        assert!(snapshot.covers(1..=10));
        assert!(snapshot.covers(40..=50));
        assert!(!snapshot.covers(20..=21));
        assert_eq!(snapshot.unseen_in(9..=12), vec![11, 12]);
    }

    /// Different content means the recorded line numbers name different text,
    /// so keeping them would let a line seen in the old file authorise an edit
    /// to the new one.
    #[test]
    fn new_content_drops_the_line_numbers_of_the_old() {
        let mut store = FileSnapshotStore::new();
        let path = Path::new("/tmp/a.rs");
        store.record(path, "one\n", Some(lines(1..=10)));
        store.record(path, "two\n", Some(lines(1..=2)));

        let snapshot = store.snapshot(path).expect("recorded");
        assert!(snapshot.covers(1..=2));
        assert!(
            !snapshot.covers(3..=3),
            "old provenance survived new content"
        );
    }

    /// A writer records `None` because the content came from the model. Once a
    /// path claims full coverage, a later partial read must not narrow it back.
    #[test]
    fn full_coverage_is_never_narrowed_by_a_partial_read() {
        let mut store = FileSnapshotStore::new();
        let path = Path::new("/tmp/a.rs");
        store.record(path, "x\n", None);
        store.record(path, "x\n", Some(lines(1..=2)));
        assert!(store.snapshot(path).expect("recorded").covers(500..=500));

        let other = Path::new("/tmp/b.rs");
        store.record(other, "y\n", Some(lines(1..=2)));
        store.record(other, "y\n", None);
        assert!(store.snapshot(other).expect("recorded").covers(500..=500));
    }

    #[test]
    fn the_failure_counter_tracks_one_attempt_not_the_file() {
        let mut store = FileSnapshotStore::new();
        let path = Path::new("/tmp/a.rs");
        assert_eq!(store.note_failure(path, "foo"), 1);
        assert_eq!(store.note_failure(path, "foo"), 2);
        assert_eq!(
            store.note_failure(path, "bar"),
            1,
            "a different edit is new"
        );
        assert_eq!(store.note_failure(path, "foo"), 3);

        store.clear_failures(path);
        assert_eq!(store.note_failure(path, "foo"), 1, "a success resets it");
    }

    #[test]
    fn forgetting_a_path_silences_both_guards_for_it() {
        let mut store = FileSnapshotStore::new();
        let path = Path::new("/tmp/a.rs");
        store.record(path, "x\n", Some(lines(1..=2)));
        store.forget(path);
        assert!(store.snapshot(path).is_none());
    }

    #[test]
    fn the_store_evicts_the_oldest_path() {
        let mut store = FileSnapshotStore::new();
        for i in 0..(MAX_PATHS + 5) {
            store.record(Path::new(&format!("/tmp/{i}.rs")), "x\n", None);
        }
        assert!(store.snapshot(Path::new("/tmp/0.rs")).is_none());
        assert!(store
            .snapshot(Path::new(&format!("/tmp/{}.rs", MAX_PATHS + 4)))
            .is_some());
        assert_eq!(store.entries.len(), MAX_PATHS);
    }
}
