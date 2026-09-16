// This module is the complete filesystem-command interface for the plugin. Every
// helper and constant here is consumed by the watcher and/or the modal.
use std::collections::BTreeMap;
use std::path::Path;
use zellij_tile::prelude::*;

pub const OP_KEY: &str = "tab_notes_op";
pub const TAB_KEY: &str = "tab_notes_tab";

pub const OP_LIST: &str = "list";
pub const OP_ENSURE_DIR: &str = "ensure_dir";
pub const OP_READ: &str = "read";
pub const OP_DELETE: &str = "delete";
pub const OP_MOVE: &str = "move";
pub const OP_CLEANUP: &str = "cleanup";
pub const SESSION_KEY: &str = "tab_notes_session";
pub const OP_MIGRATE: &str = "migrate_session";
pub const SESSION_FAILED: &str = "tab-notes:session-failed";
pub const SESSION_READY: &str = "tab-notes:session-ready";
pub const OP_EDIT: &str = "edit";

pub fn context(op: &str) -> BTreeMap<String, String> {
    let mut context = BTreeMap::new();
    context.insert(OP_KEY.to_string(), op.to_string());
    context
}

pub fn context_with_tab(op: &str, tab: &str) -> BTreeMap<String, String> {
    let mut context = context(op);
    context.insert(TAB_KEY.to_string(), tab.to_string());
    context
}

pub fn op_of(context: &BTreeMap<String, String>) -> Option<&str> {
    context.get(OP_KEY).map(String::as_str)
}

pub fn ensure_dir(dir: &Path) {
    run_command(
        &["mkdir", "-p", &dir.to_string_lossy()],
        context(OP_ENSURE_DIR),
    );
}

/// Lists the notes that exist AND are non-empty, in one command.
pub fn list_notes(dir: &Path, session: &str) {
    run_command(
        &[
            "find",
            &dir.to_string_lossy(),
            "-maxdepth",
            "1",
            // `-type f` so a directory called `something.md` is never read as a note.
            "-type",
            "f",
            "-name",
            "*.md",
            "-size",
            "+0c",
        ],
        session_context(OP_LIST, session),
    );
}

pub fn read_note(path: &Path) {
    run_command(
        &["head", "-c", "65536", &path.to_string_lossy()],
        context_with_tab(OP_READ, &path.to_string_lossy()),
    );
}

pub fn delete_note(path: &Path) {
    run_command(&["rm", "-f", &path.to_string_lossy()], context(OP_DELETE));
}

/// `-n`, never `-f`: the reconciler's collision guard is an in-memory check against a
/// listing that can be stale (a note written by an editor that has not exited yet is
/// not in it), so the filesystem, not the cache, has the last word on overwriting. A
/// refused move is benign — the chained refresh re-lists and settles into the
/// documented "collision degrades to sharing, source orphaned" behaviour.
pub fn move_note(from: &Path, to: &Path) {
    run_command(
        &["mv", "-n", &from.to_string_lossy(), &to.to_string_lossy()],
        context(OP_MOVE),
    );
}

/// Deletes the file only if it is empty. `find -maxdepth 0` targets the file itself.
pub fn delete_if_empty(path: &Path) {
    run_command(
        &[
            "find",
            &path.to_string_lossy(),
            "-maxdepth",
            "0",
            "-size",
            "0c",
            "-delete",
        ],
        context(OP_CLEANUP),
    );
}

pub fn session_context(op: &str, session: &str) -> BTreeMap<String, String> {
    let mut result = context(op);
    result.insert(SESSION_KEY.to_string(), session.to_string());
    result
}

pub fn migrate_session(from: &Path, to: &Path, session: &str) {
    run_command(
        &[
            "sh",
            "-c",
            tab_notes_core::session::MIGRATE_NOTES,
            "tab-notes-migrate",
            &from.to_string_lossy(),
            &to.to_string_lossy(),
        ],
        session_context(OP_MIGRATE, session),
    );
}
