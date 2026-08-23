//! The checks an edit passes before it is written.
//!
//! `old_string` proves its own address: text that is not in the file cannot
//! match. It proves nothing else. These checks supply the two facts it cannot
//! carry, using what the reading tools recorded in
//! [`mikmik_core::file_snapshot`]:
//!
//! * the file is still the one the session read;
//! * the session displayed the lines the edit is about to change.
//!
//! Every check is silent for a path with no record, so an edit to a file this
//! session never read behaves exactly as it did before the guard existed.
//! Enforcing read-before-edit is a separate policy and is not implemented here.
//!
//! Shared by `FileEditTool` and `BatchEditTool`, because a guard on one of them
//! is a guard the model routes around by calling the other.

use std::collections::BTreeSet;
use std::path::Path;

use mikmik_core::file_snapshot::{hash_content, REPEATED_FAILURE_LIMIT};

use crate::ToolContext;

/// How many unseen lines an error quotes back before it stops listing them.
const REVEAL_CAP: usize = 12;

/// How wide a quoted line may be before it is cut.
const REVEAL_COLUMNS: usize = 200;

/// The 1-indexed inclusive line ranges `old_norm` occupies in `content_norm`.
///
/// Both arguments must already use `\n`, matching how the editing tools match.
/// Returns every occurrence when `replace_all`, otherwise just the first.
pub(crate) fn match_line_ranges(
    content_norm: &str,
    old_norm: &str,
    replace_all: bool,
) -> Vec<std::ops::RangeInclusive<usize>> {
    if old_norm.is_empty() {
        return Vec::new();
    }
    // Lines the needle itself spans, so a multi-line `old_string` reports every
    // line it covers rather than only the one it starts on.
    let span = old_norm.matches('\n').count();

    let mut ranges = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = content_norm[from..].find(old_norm) {
        let at = from + rel;
        let start = content_norm[..at].matches('\n').count() + 1;
        ranges.push(start..=(start + span));
        if !replace_all {
            break;
        }
        // Advance by one byte past the match start rather than past its end, so
        // overlapping occurrences are still reported. The editing tools do not
        // replace overlapping matches, but reporting fewer lines than an edit
        // touches is the failure this guard exists to prevent.
        from = at + old_norm.len().max(1);
        if from >= content_norm.len() {
            break;
        }
    }
    ranges
}

/// Why an edit was refused.
pub(crate) struct Refusal {
    pub message: String,
}

/// Check one edit against what the session read.
///
/// `content` is the file as it is on disk right now. `old_string` and
/// `content` are compared on an LF-normalized view, matching the editing tools.
/// Returns `None` when the edit may proceed.
pub(crate) fn check(
    ctx: &ToolContext,
    path: &Path,
    content: &str,
    old_string: &str,
    replace_all: bool,
) -> Option<Refusal> {
    let guard = ctx.config.effective_edit_guard();
    if !guard.checks_staleness() {
        return None;
    }

    let store = ctx.file_snapshots.lock();
    // No record means this session never read the file. There is nothing to
    // hold the edit to, and refusing here would be read-before-edit
    // enforcement, which is a different policy with a different blast radius.
    let snapshot = store.snapshot(path)?;

    if snapshot.hash != hash_content(content) {
        return Some(Refusal {
            message: format!(
                "{} changed after this session read it, so the edit was not applied. \
                 Read it again and redo the change against what it says now.",
                path.display()
            ),
        });
    }

    if !guard.checks_seen_lines() {
        return None;
    }

    let content_norm = content.replace("\r\n", "\n");
    let old_norm = old_string.replace("\r\n", "\n");
    let ranges = match_line_ranges(&content_norm, &old_norm, replace_all);
    let mut unseen: BTreeSet<usize> = BTreeSet::new();
    for range in ranges {
        unseen.extend(snapshot.unseen_in(range));
    }
    if unseen.is_empty() {
        return None;
    }

    let lines: Vec<&str> = content_norm.split('\n').collect();
    let mut quoted = String::new();
    for line in unseen.iter().take(REVEAL_CAP) {
        let text = lines.get(line - 1).copied().unwrap_or("");
        if text.chars().count() > REVEAL_COLUMNS {
            let head: String = text.chars().take(REVEAL_COLUMNS).collect();
            quoted.push_str(&format!("{line}: {head}…\n"));
        } else {
            quoted.push_str(&format!("{line}: {text}\n"));
        }
    }
    let more = unseen.len().saturating_sub(REVEAL_CAP);
    if more > 0 {
        quoted.push_str(&format!("… and {more} more\n"));
    }

    Some(Refusal {
        message: format!(
            "This edit changes lines of {} that were never displayed in this session, \
             so it was not applied. Read them, check the change is still right in that \
             context, then edit again.\n\nThe lines you were about to change:\n{}",
            path.display(),
            quoted.trim_end()
        ),
    })
}

/// Turn a failed `old_string` match into an error, escalating a repeat.
///
/// The plain message is returned the first two times. From the third identical
/// failure the message stops repeating the advice that has not worked and names
/// the one action that will.
pub(crate) fn describe_failed_match(
    ctx: &ToolContext,
    path: &Path,
    old_string: &str,
    plain: String,
) -> String {
    if !ctx.config.effective_edit_guard().checks_staleness() {
        return plain;
    }
    let count = ctx.file_snapshots.lock().note_failure(path, old_string);
    if count < REPEATED_FAILURE_LIMIT {
        return plain;
    }
    format!(
        "{plain}\n\nThis is attempt {count} with the same text against this file. \
         Rewording it again will not help. Read {} and copy the target out of what \
         it actually says.",
        path.display()
    )
}

/// Record the result of a successful edit.
///
/// Call this AFTER any formatter has run, not straight after the write: a
/// formatter rewrites the file, and a record taken before it would make the
/// next edit fail as "changed after this session read it".
///
/// The session has now seen whatever it wrote, but the lines around it moved.
/// A single replacement is remapped exactly. Anything else, and any case where
/// the bytes on disk are not the bytes that were written, drops the record, so
/// the next edit is unguarded rather than guarded against line numbers that no
/// longer mean anything.
pub(crate) async fn record_applied_edit(
    ctx: &ToolContext,
    path: &Path,
    before: &str,
    written: &str,
    old_string: &str,
    new_string: &str,
    replacements: usize,
) {
    // What is on disk now, which is what the next edit will be checked against.
    let settled = match ctx.read_text(path).await {
        Ok(text) => text,
        Err(_) => {
            let mut store = ctx.file_snapshots.lock();
            store.clear_failures(path);
            store.forget(path);
            return;
        }
    };

    let mut store = ctx.file_snapshots.lock();
    store.clear_failures(path);

    // A formatter reflowed the file, so the remapped line numbers name text
    // that has moved again. Nothing here can map that, so nothing claims to.
    let remapped = (replacements == 1 && settled == written)
        .then(|| remap_seen_lines(&store, path, before, old_string, new_string))
        .flatten();

    match remapped {
        Some(seen) => store.record(path, &settled, Some(seen)),
        None => store.forget(path),
    }
}

/// Record a file the model wrote in full.
///
/// Call this after any formatter has run, for the same reason as
/// [`record_applied_edit`]. No line set is recorded: the content came from the
/// model, so the seen-line check has nothing to withhold. The stale check still
/// applies, and catches anything that changes the file afterwards.
pub(crate) async fn record_written_file(ctx: &ToolContext, path: &Path) {
    // Scoped, because a `parking_lot` guard is `!Send` and holding one across
    // the read below would make this future unusable on a multi-threaded
    // executor.
    {
        ctx.file_snapshots.lock().clear_failures(path);
    }

    match ctx.read_text(path).await {
        Ok(text) => ctx.file_snapshots.lock().record(path, &text, None),
        Err(_) => ctx.file_snapshots.lock().forget(path),
    }
}

/// Shift the recorded seen lines across one replacement.
///
/// Returns `None` when the record cannot be mapped: no record, no provenance,
/// or a needle that is no longer findable. The caller then forgets the path
/// instead of guessing.
fn remap_seen_lines(
    store: &mikmik_core::file_snapshot::FileSnapshotStore,
    path: &Path,
    before: &str,
    old_string: &str,
    new_string: &str,
) -> Option<BTreeSet<usize>> {
    let seen = store.snapshot(path)?.seen_lines.as_ref()?;

    let before_norm = before.replace("\r\n", "\n");
    let old_norm = old_string.replace("\r\n", "\n");
    let range = match_line_ranges(&before_norm, &old_norm, false).pop()?;
    let (start, end) = (*range.start(), *range.end());

    let old_lines = end - start + 1;
    let new_lines = new_string.replace("\r\n", "\n").matches('\n').count() + 1;
    let delta = new_lines as isize - old_lines as isize;

    let mut mapped: BTreeSet<usize> = BTreeSet::new();
    for &line in seen.iter() {
        if line < start {
            mapped.insert(line);
        } else if line > end {
            // A line after the replacement moves by the difference in height.
            // `saturating` cannot underflow here: `delta` never removes more
            // lines than the replaced span held.
            mapped.insert((line as isize + delta).max(1) as usize);
        }
    }
    // What the model just wrote, it has seen.
    for line in start..start + new_lines {
        mapped.insert(line);
    }
    Some(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;

    fn context(guard: &str) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ctx = allow_all_context(dir.path().to_path_buf());
        ctx.config.edit_guard = Some(guard.to_string());
        (dir, ctx)
    }

    #[test]
    fn a_multi_line_needle_reports_every_line_it_covers() {
        let content = "one\ntwo\nthree\nfour\n";
        assert_eq!(match_line_ranges(content, "two", false), vec![2..=2]);
        assert_eq!(
            match_line_ranges(content, "two\nthree", false),
            vec![2..=3],
            "a needle spanning two lines must report both"
        );
        assert_eq!(match_line_ranges(content, "missing", false), Vec::new());
    }

    #[test]
    fn replace_all_reports_every_occurrence() {
        let content = "x\ny\nx\ny\nx\n";
        assert_eq!(
            match_line_ranges(content, "x", true),
            vec![1..=1, 3..=3, 5..=5]
        );
        assert_eq!(match_line_ranges(content, "x", false), vec![1..=1]);
    }

    /// The whole point of the stale check: the file moved under the model, and
    /// `old_string` still happens to match somewhere.
    #[test]
    fn an_edit_to_a_changed_file_is_refused() {
        let (_dir, ctx) = context("stale");
        let path = Path::new("/tmp/a.rs");
        ctx.file_snapshots.lock().record(path, "let x = 1;\n", None);

        let refusal = check(&ctx, path, "let y = 2;\nlet x = 1;\n", "let x = 1;", false);
        assert!(
            refusal.is_some_and(|r| r.message.contains("changed after this session read it")),
            "a changed file was accepted"
        );
    }

    #[test]
    fn an_edit_to_the_file_that_was_read_is_allowed() {
        let (_dir, ctx) = context("stale");
        let path = Path::new("/tmp/a.rs");
        ctx.file_snapshots.lock().record(path, "let x = 1;\n", None);

        assert!(check(&ctx, path, "let x = 1;\n", "let x = 1;", false).is_none());
    }

    /// A line-ending rewrite is not a content change, because the editing tools
    /// match on an LF-normalized view and would still find the same text.
    #[test]
    fn a_line_ending_rewrite_does_not_read_as_a_changed_file() {
        let (_dir, ctx) = context("stale");
        let path = Path::new("/tmp/a.rs");
        ctx.file_snapshots.lock().record(path, "a\nb\n", None);

        assert!(check(&ctx, path, "a\r\nb\r\n", "a", false).is_none());
    }

    #[test]
    fn a_file_this_session_never_read_is_not_guarded() {
        let (_dir, ctx) = context("strict");
        assert!(check(
            &ctx,
            Path::new("/tmp/unread.rs"),
            "anything\n",
            "any",
            false
        )
        .is_none());
    }

    #[test]
    fn the_off_level_checks_nothing() {
        let (_dir, ctx) = context("off");
        let path = Path::new("/tmp/a.rs");
        ctx.file_snapshots.lock().record(path, "old\n", None);
        assert!(check(&ctx, path, "completely different\n", "different", false).is_none());
    }

    /// The blind edit: a partial read leaves most of the file undisplayed, and
    /// `old_string` matches a line the model never saw.
    #[test]
    fn an_edit_to_a_line_that_was_never_displayed_is_refused() {
        let (_dir, ctx) = context("strict");
        let path = Path::new("/tmp/a.rs");
        let content = "one\ntwo\nthree\nfour\nfive\n";
        ctx.file_snapshots
            .lock()
            .record(path, content, Some((1..=2).collect()));

        let refusal = check(&ctx, path, content, "four", false).expect("blind edit accepted");
        assert!(
            refusal.message.contains("never displayed"),
            "{}",
            refusal.message
        );
        assert!(
            refusal.message.contains("4: four"),
            "the error must quote the line, got {}",
            refusal.message
        );
    }

    #[test]
    fn an_edit_inside_the_displayed_range_is_allowed() {
        let (_dir, ctx) = context("strict");
        let path = Path::new("/tmp/a.rs");
        let content = "one\ntwo\nthree\n";
        ctx.file_snapshots
            .lock()
            .record(path, content, Some((1..=3).collect()));

        assert!(check(&ctx, path, content, "two", false).is_none());
    }

    /// `stale` must not reject a blind edit, or the ladder has only one rung.
    #[test]
    fn the_stale_level_allows_an_undisplayed_line() {
        let (_dir, ctx) = context("stale");
        let path = Path::new("/tmp/a.rs");
        let content = "one\ntwo\nthree\n";
        ctx.file_snapshots
            .lock()
            .record(path, content, Some((1..=1).collect()));

        assert!(check(&ctx, path, content, "three", false).is_none());
    }

    #[test]
    fn a_repeated_failure_says_something_new_the_third_time() {
        let (_dir, ctx) = context("stale");
        let path = Path::new("/tmp/a.rs");
        let plain = "old_string not found".to_string();

        assert_eq!(
            describe_failed_match(&ctx, path, "foo", plain.clone()),
            plain
        );
        assert_eq!(
            describe_failed_match(&ctx, path, "foo", plain.clone()),
            plain
        );
        let third = describe_failed_match(&ctx, path, "foo", plain.clone());
        assert!(third.contains("attempt 3"), "{third}");
        assert!(third.contains("Read"), "{third}");
    }

    #[test]
    fn the_off_level_never_escalates() {
        let (_dir, ctx) = context("off");
        let path = Path::new("/tmp/a.rs");
        let plain = "old_string not found".to_string();
        for _ in 0..5 {
            assert_eq!(
                describe_failed_match(&ctx, path, "foo", plain.clone()),
                plain
            );
        }
    }

    /// A real file on disk, because `record_applied_edit` reads the path back
    /// to see what a formatter left behind. Against a path that does not exist
    /// the helper forgets the record, and every later assertion then passes
    /// because nothing is guarded rather than because the record survived.
    fn staged(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).expect("stage file");
        path
    }

    /// After an edit the file is still the file the session knows, and the
    /// lines below the change have moved. Both facts have to survive, or the
    /// next edit is refused for the wrong reason.
    #[tokio::test]
    async fn an_applied_edit_keeps_the_record_usable() {
        let (dir, ctx) = context("strict");
        let before = "one\ntwo\nthree\n";
        let after = "one\nTWO\nEXTRA\nthree\n";
        let path = staged(&dir, "a.rs", after);
        ctx.file_snapshots
            .lock()
            .record(&path, before, Some((1..=3).collect()));

        record_applied_edit(&ctx, &path, before, after, "two", "TWO\nEXTRA", 1).await;

        assert!(
            ctx.file_snapshots.lock().snapshot(&path).is_some(),
            "the record was dropped, so the assertions below prove nothing"
        );
        // The stale check now passes against the new content.
        assert!(check(&ctx, &path, after, "TWO", false).is_none());
        // `three` moved from line 3 to line 4 and is still seen.
        assert!(check(&ctx, &path, after, "three", false).is_none());
        // A line the read never displayed is still refused.
        assert!(check(&ctx, &path, after, "EXTRA\nthree", false).is_none());
    }

    /// A formatter rewrites the file after the edit lands. The record has to
    /// describe what is on disk, or the very next edit reads as stale.
    #[tokio::test]
    async fn a_formatter_rewriting_the_file_does_not_leave_a_stale_record() {
        let (dir, ctx) = context("stale");
        let written = "one\nTWO\n";
        // What a formatter left behind, which is not what the tool wrote.
        let formatted = "one\nTWO;\n";
        let path = staged(&dir, "a.rs", formatted);
        ctx.file_snapshots
            .lock()
            .record(&path, "one\ntwo\n", Some((1..=2).collect()));

        record_applied_edit(&ctx, &path, "one\ntwo\n", written, "two", "TWO", 1).await;

        assert!(
            check(&ctx, &path, formatted, "TWO;", false).is_none(),
            "the next edit reads as stale because the record predates the formatter"
        );
    }

    /// A replacement the remap cannot follow drops the record instead of
    /// keeping line numbers that no longer name the same text.
    #[tokio::test]
    async fn a_multi_replacement_edit_drops_the_record() {
        let (dir, ctx) = context("strict");
        let path = staged(&dir, "a.rs", "y\ny\n");
        ctx.file_snapshots
            .lock()
            .record(&path, "x\nx\n", Some((1..=2).collect()));

        record_applied_edit(&ctx, &path, "x\nx\n", "y\ny\n", "x", "y", 2).await;
        assert!(ctx.file_snapshots.lock().snapshot(&path).is_none());
    }

    #[tokio::test]
    async fn a_successful_edit_clears_the_failure_counter() {
        let (dir, ctx) = context("stale");
        let path = staged(&dir, "a.rs", "b\n");
        let plain = "nope".to_string();
        describe_failed_match(&ctx, &path, "foo", plain.clone());
        describe_failed_match(&ctx, &path, "foo", plain.clone());

        record_applied_edit(&ctx, &path, "a\n", "b\n", "a", "b", 1).await;

        assert_eq!(
            describe_failed_match(&ctx, &path, "foo", plain.clone()),
            plain,
            "the counter survived a successful edit"
        );
    }

    /// The writing tool records what is on disk after formatting, so the file
    /// it just created is guarded from the next turn on.
    #[tokio::test]
    async fn a_written_file_is_recorded_from_disk() {
        let (dir, ctx) = context("strict");
        let path = staged(&dir, "new.rs", "fn main() {}\n");

        record_written_file(&ctx, &path).await;

        let store = ctx.file_snapshots.lock();
        let snapshot = store.snapshot(&path).expect("the write recorded nothing");
        assert_eq!(snapshot.hash, hash_content("fn main() {}\n"));
        assert!(
            snapshot.covers(1..=500),
            "the model wrote this file, so no line is withheld from it"
        );
    }
}
