use crate::fs_ops;
use std::collections::BTreeSet;
use tab_notes_core::config::Config;
use tab_notes_core::listing::parse_note_listing;
use tab_notes_core::paths::{note_path_from_key, session_dir};
use tab_notes_core::reconcile::{Action, Reconciler, TabView};
use zellij_tile::prelude::*;

pub struct Watcher {
    config: Result<Config, String>,
    reconciler: Option<Reconciler>,
    session: Option<String>,
    requested_session: Option<String>,
    migrating: bool,
    pending_moves: usize,
    permitted: bool,
    tabs: Vec<TabView>,
    notes: BTreeSet<String>,
    /// Whether a listing has ever completed. Until it has, `notes` is empty because
    /// nothing has been read yet — not because no tab has a note — and acting on it
    /// would strip the icon off every tab a previous session had decorated, only to
    /// put it back once the listing lands.
    listed_once: bool,
}

impl Watcher {
    pub fn new(config: Result<Config, String>) -> Self {
        let reconciler = config
            .as_ref()
            .ok()
            .map(|c| Reconciler::new(c.icon.clone()));
        Self {
            config,
            reconciler,
            session: None,
            requested_session: None,
            migrating: false,
            pending_moves: 0,
            permitted: false,
            tabs: Vec::new(),
            notes: BTreeSet::new(),
            listed_once: false,
        }
    }

    pub fn load(&mut self) {
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::SessionUpdate,
            EventType::TabUpdate,
            EventType::RunCommandResult,
        ]);
    }

    pub fn update(&mut self, event: Event) -> bool {
        if self.config.is_err() {
            return false;
        }
        match event {
            Event::SessionUpdate(sessions, _) => {
                if let Some(current) = sessions.iter().find(|s| s.is_current_session) {
                    if self.requested_session.as_ref() != Some(&current.name) {
                        self.requested_session = Some(current.name.clone());
                        self.sync_session();
                    }
                }
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs
                    .iter()
                    .map(|tab| TabView {
                        id: tab.tab_id,
                        name: tab.name.clone(),
                    })
                    .collect();
                self.apply();
            }
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                self.on_command_result(exit_code, stdout, stderr, context);
            }
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permitted = true;
                self.sync_session();
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                // Reuse the "not configured" inert path rather than adding a second flag:
                // every guard in this struct already checks it.
                eprintln!("tab-notes: permissions denied, the watcher will stay inert");
                self.config = Err("tab-notes: permissions denied".to_string());
            }
            _ => {}
        }
        false
    }

    fn on_command_result(
        &mut self,
        exit_code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        context: std::collections::BTreeMap<String, String>,
    ) {
        match fs_ops::op_of(&context) {
            Some(fs_ops::OP_ENSURE_DIR) => self.refresh(),
            Some(fs_ops::OP_MIGRATE) => {
                self.migrating = false;
                if exit_code != Some(0) {
                    eprintln!(
                        "tab-notes: session migration failed: {}",
                        String::from_utf8_lossy(&stderr)
                    );
                    if let Some(session) = context.get(fs_ops::SESSION_KEY) {
                        pipe_message_to_plugin(
                            MessageToPlugin::new(fs_ops::SESSION_FAILED)
                                .with_payload(session.clone()),
                        );
                    }
                    return;
                }
                self.session = context.get(fs_ops::SESSION_KEY).cloned();
                self.sync_session();
            }
            Some(fs_ops::OP_LIST) => {
                if self.migrating || context.get(fs_ops::SESSION_KEY) != self.session.as_ref() {
                    return;
                }
                // A non-zero exit means the directory does not exist yet: no notes.
                self.notes = if exit_code == Some(0) {
                    parse_note_listing(&String::from_utf8_lossy(&stdout))
                } else {
                    eprintln!(
                        "tab-notes: listing failed: {}",
                        String::from_utf8_lossy(&stderr)
                    );
                    BTreeSet::new()
                };
                self.listed_once = true;
                self.apply();
                if exit_code == Some(0) {
                    if let Some(session) = &self.session {
                        pipe_message_to_plugin(
                            MessageToPlugin::new(fs_ops::SESSION_READY)
                                .with_payload(session.clone()),
                        );
                    }
                }
            }
            Some(op @ (fs_ops::OP_MOVE | fs_ops::OP_DELETE)) => {
                if exit_code != Some(0) {
                    eprintln!(
                        "tab-notes: {op} failed: {}",
                        String::from_utf8_lossy(&stderr)
                    );
                }
                if op == fs_ops::OP_MOVE {
                    self.pending_moves = self.pending_moves.saturating_sub(1);
                }
                self.sync_session();
            }
            // `find … -size 0c -delete` exits non-zero when the file was never
            // created, which is the routine "opened a note and quit without saving"
            // path. Refresh, but do not call it an error.
            Some(fs_ops::OP_CLEANUP) => self.refresh(),
            _ => {}
        }
    }

    // Only one migration runs at a time. If another rename arrives while it is
    // running, finish the current move before migrating to the latest name.
    fn sync_session(&mut self) {
        if !self.permitted || self.migrating || self.pending_moves > 0 {
            return;
        }
        let (Ok(config), Some(requested)) = (&self.config, &self.requested_session) else {
            return;
        };
        match &self.session {
            Some(current) if current != requested => {
                self.migrating = true;
                self.listed_once = false;
                fs_ops::migrate_session(
                    &session_dir(&config.notes_dir, current),
                    &session_dir(&config.notes_dir, requested),
                    requested,
                );
            }
            None => {
                self.session = Some(requested.clone());
                fs_ops::ensure_dir(&session_dir(&config.notes_dir, requested));
            }
            _ => self.refresh(),
        }
    }

    /// Re-reads the notes directory. Everything downstream flows from the result.
    pub fn refresh(&mut self) {
        let (Ok(config), Some(session)) = (self.config.as_ref(), self.session.as_ref()) else {
            return;
        };
        if self.migrating || self.session != self.requested_session {
            return;
        }
        fs_ops::list_notes(&session_dir(&config.notes_dir, session), session);
    }

    fn apply(&mut self) {
        // An empty `notes` before the first listing means "not known yet", not "no
        // notes": reconciling against it would strip every icon, then restore it when
        // the listing arrives.
        if !self.listed_once || self.migrating || self.session != self.requested_session {
            return;
        }
        let (Ok(config), Some(session), Some(reconciler)) = (
            self.config.as_ref(),
            self.session.as_ref(),
            self.reconciler.as_mut(),
        ) else {
            return;
        };
        for action in reconciler.reconcile(&self.tabs, &mut self.notes) {
            match action {
                // `rename_tab` takes a 1-based position and subtracts one internally;
                // `rename_tab_with_id` looks the tab up directly, with no arithmetic.
                Action::RenameTab { id, name } => rename_tab_with_id(id as u64, &name),
                // Both endpoints are already note keys: sanitizing them again is not
                // a no-op and would point the move at a different file.
                Action::MoveNote { from, to } => {
                    self.pending_moves += 1;
                    fs_ops::move_note(
                        &note_path_from_key(&config.notes_dir, session, &from),
                        &note_path_from_key(&config.notes_dir, session, &to),
                    );
                }
            }
        }
    }

    /// The modal owns the editor, not the watcher: `EditPaneExited` is delivered only to
    /// the plugin that opened the file, and the modal needs it to show the edited note
    /// again instead of vanishing. All the watcher still wants to hear is that the notes
    /// directory changed.
    pub fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if self.config.is_err() || self.session.is_none() {
            return false;
        }
        if pipe_message.name == "tab-notes:notes-changed" {
            self.sync_session();
        }
        false
    }
}
