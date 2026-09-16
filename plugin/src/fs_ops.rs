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

/// Inventory includes empty files, symlinks and directories as reserved names;
/// only non-empty regular files count as notes for the tab marker.
pub fn list_notes(dir: &Path, session: &str) {
    run_command(
        &[
            "sh",
            "-c",
            r#"
set -eu
[ -d "$1" ]
for note in "$1"/*.md; do
    [ -e "$note" ] || [ -L "$note" ] || continue
    printf 'P%s\n' "$note"
    if [ -f "$note" ] && [ ! -L "$note" ] && [ -s "$note" ]; then
        printf 'N%s\n' "$note"
    fi
done
"#,
            "tab-notes-list",
            &dir.to_string_lossy(),
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

/// Keep the original owner if a destination appeared after the last listing.
pub fn move_note(from: &Path, to: &Path, id: usize, from_key: &str) {
    let mut context = context_with_tab(OP_MOVE, &id.to_string());
    context.insert("from_key".to_string(), from_key.to_string());
    run_command(
        &[
            "sh",
            "-c",
            r#"
set -eu
[ ! -e "$2" ] && [ ! -L "$2" ] || exit 17
mv -n "$1" "$2"
[ ! -e "$1" ] && [ ! -L "$1" ] || exit 17
"#,
            "tab-notes-move",
            &from.to_string_lossy(),
            &to.to_string_lossy(),
        ],
        context,
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
