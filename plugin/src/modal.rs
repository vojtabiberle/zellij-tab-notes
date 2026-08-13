use crate::fs_ops;
use tab_notes_core::config::Config;
use tab_notes_core::icon::strip_icon;
use tab_notes_core::markdown::{self, LineKind, RenderedLine, SpanKind};
use tab_notes_core::paths::note_path;
use tab_notes_core::viewport::clamp_scroll;
use zellij_tile::prelude::*;

/// Percentages of the screen. Expanding cannot restore whatever size Zellij originally
/// chose — a plugin has no way to read its own coordinates — so expanding means "this
/// size", and from the first minimise on, that is the modal's size.
#[derive(Clone, Copy)]
struct Geometry {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    /// Percentages of the screen when true, terminal cells when false.
    percent: bool,
}

/// Only used until the modal has seen its own geometry — from then on it restores the
/// exact size it had, because a guessed percentage is visibly not the size Zellij gave
/// it.
const EXPANDED_FALLBACK: Geometry = Geometry {
    x: 10,
    y: 10,
    width: 80,
    height: 70,
    percent: true,
};

const MINIMIZED: Geometry = Geometry {
    x: 72,
    y: 0,
    width: 28,
    height: 32,
    percent: true,
};

pub struct Modal {
    config: Result<Config, String>,
    session: Option<String>,
    /// Stable ownership discovered from this pane's position in `PaneManifest`.
    /// Once assigned, a modal must never follow whichever other tab becomes active.
    tab_id: Option<usize>,
    tabs: Vec<TabInfo>,
    tab: Option<String>,
    content: Option<String>,
    status: Option<String>,
    scroll: usize,
    confirming_delete: bool,
    minimized: bool,
    /// The geometry to go back to, as last seen while expanded.
    expanded: Option<Geometry>,
    focused: bool,
    /// The tiled pane to hand focus back to, and the tab it lives in.
    tab_position: Option<usize>,
    terminal_pane: Option<u32>,
    /// A second `LaunchPlugin` in this tab focuses the incumbent and closes itself.
    /// Ignore further events once that hand-off has started.
    closing_duplicate: bool,
}

impl Modal {
    pub fn new(config: Result<Config, String>) -> Self {
        Self {
            config,
            session: None,
            tab_id: None,
            tabs: Vec::new(),
            tab: None,
            content: None,
            status: None,
            scroll: 0,
            confirming_delete: false,
            minimized: false,
            expanded: None,
            focused: false,
            tab_position: None,
            terminal_pane: None,
            closing_duplicate: false,
        }
    }

    pub fn load(&mut self) {
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::SessionUpdate,
            EventType::TabUpdate,
            EventType::RunCommandResult,
            EventType::EditPaneExited,
            EventType::PaneUpdate,
            EventType::Key,
        ]);
    }

    pub fn update(&mut self, event: Event) -> bool {
        if self.closing_duplicate {
            return false;
        }
        let Ok(config) = self.config.clone() else {
            return false;
        };
        match event {
            Event::SessionUpdate(sessions, _) => {
                let Some(current) = sessions.iter().find(|s| s.is_current_session) else {
                    return false;
                };
                if self.session.as_deref() != Some(current.name.as_str()) {
                    self.session = Some(current.name.clone());
                    // Note-scoped state must not survive a change of which note is
                    // shown. `content` included: while the new read is in flight it
                    // would otherwise answer `has_note()` about the previous note
                    // while `delete_note()` already targets the new one.
                    self.confirming_delete = false;
                    self.status = None;
                    self.content = None;
                    self.read_note();
                }
                true
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                self.sync_owned_tab(&config)
            }
            Event::RunCommandResult(exit_code, stdout, stderr, context) => {
                match fs_ops::op_of(&context) {
                    Some(fs_ops::OP_READ) => {
                        self.content = if exit_code == Some(0) {
                            Some(String::from_utf8_lossy(&stdout).to_string())
                        } else {
                            None
                        };
                        true
                    }
                    // Report what actually happened, and only tell the watcher the
                    // note is gone once the `rm` has really finished — sending the
                    // pipe from `delete_note` raced the subprocess, so the refresh
                    // could list the note that was still there and leave the icon on.
                    Some(fs_ops::OP_DELETE) => {
                        if exit_code == Some(0) {
                            self.content = None;
                            self.status = Some("note deleted".to_string());
                            Self::send_to_watcher("tab-notes:notes-changed", None);
                        } else {
                            eprintln!(
                                "tab-notes: delete failed: {}",
                                String::from_utf8_lossy(&stderr)
                            );
                            self.status = Some("delete failed — see the Zellij log".to_string());
                        }
                        true
                    }
                    // The post-edit cleanup finished: re-read so the preview shows what
                    // was just written, and tell the watcher in case the note came into
                    // existence or stopped existing.
                    Some(fs_ops::OP_CLEANUP) => {
                        self.read_note();
                        Self::send_to_watcher("tab-notes:notes-changed", None);
                        true
                    }
                    _ => false,
                }
            }
            // Only ever delivered for a pane this plugin opened, but the op tag keeps that
            // true even if the modal grows another kind of pane later.
            Event::EditPaneExited(_pane_id, _exit_code, context) => {
                if fs_ops::op_of(&context) != Some(fs_ops::OP_EDIT) {
                    return false;
                }
                let (Some(session), Some(tab)) = (self.session.as_ref(), self.tab.as_ref()) else {
                    return false;
                };
                // An aborted edit leaves a zero-byte file behind; remove it so it never
                // counts as a note. Re-reading is chained to that command's result.
                fs_ops::delete_if_empty(&note_path(&config.notes_dir, session, tab));
                self.status = None;
                true
            }
            // Remembering the real geometry is the only way to restore it: a plugin
            // cannot read its own coordinates on demand, and a guessed percentage comes
            // back visibly the wrong size.
            Event::PaneUpdate(manifest) => {
                let me = get_plugin_ids().plugin_id;
                let Some((pane_tab_position, pane)) =
                    manifest.panes.iter().find_map(|(position, panes)| {
                        panes
                            .iter()
                            .find(|pane| pane.is_plugin && pane.id == me)
                            .map(|pane| (*position, pane))
                    })
                else {
                    return false;
                };

                // `LaunchPlugin` deliberately creates a new instance. If this tab
                // already has the same floating modal, hand focus to the older pane
                // and discard only this transient duplicate. The watcher is suppressed
                // and has a different alias, so it can never be selected here.
                if let Some(plugin_url) = pane.plugin_url.as_ref() {
                    let incumbent = manifest
                        .panes
                        .get(&pane_tab_position)
                        .into_iter()
                        .flatten()
                        .filter(|candidate| {
                            candidate.is_plugin
                                && candidate.is_floating
                                && !candidate.is_suppressed
                                && candidate.id < me
                                && candidate.plugin_url.as_ref() == Some(plugin_url)
                        })
                        .map(|candidate| candidate.id)
                        .min();
                    if let Some(incumbent) = incumbent {
                        self.closing_duplicate = true;
                        focus_plugin_pane(incumbent, true, false);
                        close_self();
                        return false;
                    }
                }

                // PaneManifest is authoritative about where this particular plugin
                // instance was created. Use that position only to discover the stable
                // tab id; subsequent tab switches or reordering cannot change owner.
                if self.tab_id.is_none() {
                    self.tab_position = Some(pane_tab_position);
                    self.sync_owned_tab(&config);
                }
                if !self.minimized {
                    self.expanded = Some(Geometry {
                        x: pane.pane_x,
                        y: pane.pane_y,
                        width: pane.pane_columns,
                        height: pane.pane_rows,
                        percent: false,
                    });
                }
                // Zellij tracks tiled focus separately from floating focus, so the tiled
                // pane still flagged focused is the one the user came from. Fall back to
                // the first usable terminal in the tab.
                if let Some(panes) = self.tab_position.and_then(|pos| manifest.panes.get(&pos)) {
                    let usable = |pane: &&PaneInfo| {
                        !pane.is_plugin && !pane.is_floating && pane.is_selectable && !pane.exited
                    };
                    self.terminal_pane = panes
                        .iter()
                        .find(|pane| usable(pane) && pane.is_focused)
                        .or_else(|| panes.iter().find(usable))
                        .map(|pane| pane.id);
                }
                // Minimised, the key hints are only worth their row while the box can
                // actually receive those keys.
                let changed = self.focused != pane.is_focused;
                self.focused = pane.is_focused;
                changed
            }
            Event::Key(key) => self.on_key(key),
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.config = Err(
                    "tab-notes: permissions denied — close this pane, reload the plugin \
                     and accept the request"
                        .to_string(),
                );
                true
            }
            _ => false,
        }
    }

    fn sync_owned_tab(&mut self, config: &Config) -> bool {
        let target = match self.tab_id {
            Some(tab_id) => self.tabs.iter().find(|tab| tab.tab_id == tab_id),
            None => self
                .tab_position
                .and_then(|position| self.tabs.iter().find(|tab| tab.position == position)),
        };
        let Some(target) = target else {
            return false;
        };
        let tab_id = target.tab_id;
        let tab_position = target.position;
        let clean = strip_icon(&target.name, &config.icon).to_string();

        self.tab_id = Some(tab_id);
        self.tab_position = Some(tab_position);
        if self.tab.as_deref() == Some(clean.as_str()) {
            return false;
        }

        self.tab = Some(clean);
        self.scroll = 0;
        // Note-scoped state must not survive a tab rename. `content` included: while
        // the new read is in flight it would otherwise answer `has_note()` about the
        // previous name while `delete_note()` already targets the new one.
        self.confirming_delete = false;
        self.status = None;
        self.content = None;
        self.read_note();
        true
    }

    fn read_note(&mut self) {
        let (Ok(config), Some(session), Some(tab)) = (
            self.config.as_ref(),
            self.session.as_ref(),
            self.tab.as_ref(),
        ) else {
            return;
        };
        fs_ops::read_note(&note_path(&config.notes_dir, session, tab));
    }

    fn open_editor(&mut self) {
        let (Ok(config), Some(session), Some(tab)) = (
            self.config.as_ref(),
            self.session.as_ref(),
            self.tab.as_ref(),
        ) else {
            return;
        };
        open_file_floating(
            FileToOpen::new(note_path(&config.notes_dir, session, tab)),
            None,
            fs_ops::context_with_tab(fs_ops::OP_EDIT, tab),
        );
        self.status = Some("editing in $EDITOR…".to_string());
    }

    fn own_pane(&self) -> PaneId {
        PaneId::Plugin(get_plugin_ids().plugin_id)
    }

    fn apply_geometry(&self, geometry: Geometry) {
        let unit = if geometry.percent { "%" } else { "" };
        let Some(coordinates) = FloatingPaneCoordinates::new(
            Some(format!("{}{unit}", geometry.x)),
            Some(format!("{}{unit}", geometry.y)),
            Some(format!("{}{unit}", geometry.width)),
            Some(format!("{}{unit}", geometry.height)),
            None,
            None,
        ) else {
            eprintln!("tab-notes: could not build the pane coordinates");
            return;
        };
        change_floating_panes_coordinates(vec![(self.own_pane(), coordinates)]);
    }

    fn has_note(&self) -> bool {
        self.content.as_ref().is_some_and(|c| !c.trim().is_empty())
    }

    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Esc | BareKey::Char('q') => {
                close_self();
                false
            }
            BareKey::Char('j') | BareKey::Down => {
                self.scroll = self.scroll.saturating_add(1);
                true
            }
            BareKey::Char('k') | BareKey::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            BareKey::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                true
            }
            BareKey::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                true
            }
            // Without a tab name there is nothing to open: the watcher drops a
            // payload-less `edit-note` silently, so the modal would just close.
            // The modal opens the editor itself and stays alive behind it. Zellij delivers
            // `EditPaneExited` only to the plugin that opened the file, and that event is
            // what lets the modal show the edited note instead of disappearing.
            BareKey::Char('e') if self.tab.is_some() => {
                self.open_editor();
                true
            }
            // Toggles a pinned corner box that stays readable while you work
            // elsewhere. Ctrl t, a focuses it again; m brings it back to full size.
            BareKey::Char('m') => {
                self.minimized = !self.minimized;
                if self.minimized {
                    self.apply_geometry(MINIMIZED);
                    set_floating_pane_pinned(self.own_pane(), true);
                } else {
                    // Unpin first: a pinned pane may well ignore a coordinate change.
                    set_floating_pane_pinned(self.own_pane(), false);
                    self.apply_geometry(self.expanded.unwrap_or(EXPANDED_FALLBACK));
                }
                true
            }
            // Hands focus back so you can keep working, leaving the box where it is —
            // pinned and minimised, it stays readable from the terminal.
            BareKey::Char('f') => {
                // focus_previous_pane only cycles the tiled focus and cannot move focus
                // out of a floating pane. Focusing a pane by id does, and it hides the
                // floating panes — which leaves a pinned box on screen, since Zellij
                // renders floating panes when any of them is pinned.
                if let Some(id) = self.terminal_pane {
                    focus_terminal_pane(id, false, false);
                }
                false
            }
            BareKey::Char('d') if self.has_note() && !self.confirming_delete => {
                self.confirming_delete = true;
                true
            }
            BareKey::Char('y') if self.confirming_delete => {
                self.confirming_delete = false;
                self.delete_note();
                true
            }
            BareKey::Char('n') if self.confirming_delete => {
                self.confirming_delete = false;
                true
            }
            _ => false,
        }
    }

    /// Broadcasts to every plugin in the session, which is how the background watcher is
    /// reached.
    ///
    /// The watcher's plugin id cannot be discovered: `SessionInfo.plugins` is empty in the
    /// `SessionUpdate` delivered to plugins, so an id-addressed message had nowhere to go and
    /// the modal closed doing nothing. A `MessageToPlugin` carrying neither a url nor a
    /// destination id is routed to all plugin ids, headless ones included. Other plugins
    /// ignore a pipe name they do not know, and the modal ignores pipes entirely, so only the
    /// watcher acts on it. Names are prefixed because every plugin now sees them.
    fn send_to_watcher(name: &str, payload: Option<String>) {
        let mut message = MessageToPlugin::new(name);
        if let Some(payload) = payload {
            message = message.with_payload(payload);
        }
        pipe_message_to_plugin(message);
    }

    fn delete_note(&mut self) {
        let (Ok(config), Some(session), Some(tab)) = (
            self.config.as_ref(),
            self.session.as_ref(),
            self.tab.as_ref(),
        ) else {
            return;
        };
        // The modal performs its own destructive operation so that deleting still works
        // when no watcher is loaded; the watcher is only told to refresh the icon.
        // Everything the user is told about the outcome happens in the OP_DELETE arm
        // of `update`, once the command has actually reported an exit code.
        fs_ops::delete_note(&note_path(&config.notes_dir, session, tab));
        self.status = Some("deleting…".to_string());
    }

    pub fn render(&mut self, rows: usize, cols: usize) {
        if let Err(error) = &self.config {
            print_text_with_coordinates(Text::new(error), 0, 0, Some(cols), None);
            return;
        }
        let title = match &self.tab {
            Some(tab) => format!("Note · {tab}"),
            None => "Note".to_string(),
        };
        print_text_with_coordinates(Text::new(&title).color_range(2, ..), 0, 0, Some(cols), None);

        // Minimised there is no room to spend on a key list, and the keys it would
        // advertise need focus anyway.
        let body_rows = if self.minimized && !self.focused {
            rows.saturating_sub(2)
        } else {
            rows.saturating_sub(3)
        };
        match &self.content {
            Some(content) if self.has_note() => {
                let lines = markdown::wrap(&markdown::render(content), cols);
                self.scroll = clamp_scroll(self.scroll, lines.len(), body_rows);
                for (row, line) in lines.iter().skip(self.scroll).take(body_rows).enumerate() {
                    print_text_with_coordinates(style(line, cols), 0, row + 2, Some(cols), None);
                }
            }
            _ => {
                let empty = match &self.tab {
                    Some(tab) => format!("No note for «{tab}» — press e to create one"),
                    None => "Loading…".to_string(),
                };
                print_text_with_coordinates(Text::new(empty).dim_all(), 0, 2, Some(cols), None);
            }
        }

        if self.minimized && !self.focused {
            return;
        }

        let footer = match (&self.status, self.confirming_delete) {
            (_, true) => "delete this note? y/n".to_string(),
            (Some(status), _) => status.clone(),
            _ if self.minimized => "m restore · f terminal".to_string(),
            _ => "e edit · d delete · m minimise · f terminal · j/k scroll · Esc close".to_string(),
        };
        print_text_with_coordinates(
            Text::new(footer).dim_all(),
            0,
            rows.saturating_sub(1),
            Some(cols),
            None,
        );
    }
}

/// Maps a line's meaning onto Zellij's styling primitives.
///
/// A terminal has one font size, so headings read as headings through case, colour and
/// the rule underneath them, not through size. Note that `Text` is bold by default —
/// there is no `bold_range`, only `unbold_*` — so body text is explicitly unbolded and
/// weight is what sets headings and strong runs apart.
fn style(line: &RenderedLine, cols: usize) -> Text {
    match line.kind {
        LineKind::Rule => Text::new("─".repeat(cols)).dim_all(),
        LineKind::Heading1 => Text::new(&line.text).color_range(0, ..),
        LineKind::Heading2 => Text::new(&line.text).color_range(3, ..),
        LineKind::Heading3 => Text::new(&line.text).color_range(1, ..),
        // A finished task is still readable but stops competing for attention.
        LineKind::Checkbox { done: true } => body(line).dim_all(),
        LineKind::Checkbox { done: false } => body(line).color_range(2, 0..1),
        LineKind::Bullet => body(line).color_range(1, 0..1),
        LineKind::Quote => body(line).dim_all(),
        LineKind::Code => Text::new(&line.text).color_range(1, ..).unbold_all(),
        LineKind::Plain => body(line),
    }
}

/// Unbolds everything except the strong runs, and colours the inline code runs.
fn body(line: &RenderedLine) -> Text {
    let mut text = Text::new(&line.text);
    let len = line.text.chars().count();
    let strong: Vec<(usize, usize)> = line
        .spans
        .iter()
        .filter(|s| s.kind == SpanKind::Strong)
        .map(|s| (s.start, s.end))
        .collect();

    if strong.is_empty() {
        text = text.unbold_all();
    } else {
        // There is no way to add weight, only to remove it, so the strong runs are the
        // gaps left between the ranges we unbold.
        let mut cursor = 0;
        for (start, end) in &strong {
            if cursor < *start {
                text = text.unbold_range(cursor..*start);
            }
            cursor = *end;
        }
        if cursor < len {
            text = text.unbold_range(cursor..len);
        }
    }

    for span in line.spans.iter().filter(|s| s.kind == SpanKind::Code) {
        text = text.color_range(1, span.start..span.end);
    }
    text
}
