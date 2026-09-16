use crate::icon::{decorate, strip_icon};
use crate::paths::sanitize_tab_name;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub id: usize,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Rename the tab with this `id` so its name matches whether it has a note.
    ///
    /// The id, not the position: `ScreenInstruction::RenameTab` treats its index as
    /// 1-based user input and subtracts one, so passing a 0-based `TabInfo.position`
    /// renames the tab before the intended one — and never converges.
    RenameTab { id: usize, name: String },
    /// Move a note file because its tab was renamed. Both values are **note keys**
    /// (`sanitize_tab_name` applied to a clean tab name), not display names, so they
    /// must be turned into paths with `note_path_from_key`.
    MoveNote { id: usize, from: String, to: String },
}

/// Keeps tab names in sync with the set of tabs that have notes.
///
/// Two namespaces meet here and must not be confused:
///
/// - the **display name** is the user's tab name with the icon stripped, and is what
///   goes back to Zellij in `Action::RenameTab`;
/// - the **note key** is `sanitize_tab_name(display_name)`, and is what the `notes`
///   set contains, because that set is parsed from filenames.
///
/// Looking a display name up in `notes` silently fails for anything sanitization
/// rewrites (`feature/login` vs `feature-login`): the tab never gets an icon, and the
/// collision guard below is defeated, so a rename can destroy another tab's note.
///
/// `reconcile` is idempotent: feeding it a settled state produces no actions, which is
/// what stops a rename from re-triggering itself through the resulting `TabUpdate`.
pub struct Reconciler {
    icon: String,
    /// tab id -> last seen note key. The id is stable across MoveTab and across the
    /// closing of other tabs, which is what makes rename detection trustworthy.
    known: BTreeMap<usize, String>,
}

impl Reconciler {
    pub fn new(icon: impl Into<String>) -> Self {
        Self {
            icon: icon.into(),
            known: BTreeMap::new(),
        }
    }

    /// A failed no-clobber move must retain its original owner for the retry.
    pub fn restore_note_key(&mut self, id: usize, key: String) {
        self.known.insert(id, key);
    }

    pub fn reconcile(&mut self, tabs: &[TabView], notes: &mut BTreeSet<String>) -> Vec<Action> {
        self.reconcile_with_occupied(tabs, notes, &notes.clone())
    }

    pub fn reconcile_with_occupied(
        &mut self,
        tabs: &[TabView],
        notes: &mut BTreeSet<String>,
        occupied: &BTreeSet<String>,
    ) -> Vec<Action> {
        // Resolve ownership before moving anything. Established owners win over
        // incoming renames; stable IDs break ties independently of display order.
        let mut ordered: Vec<_> = tabs.iter().collect();
        ordered.sort_by_key(|tab| {
            let key = sanitize_tab_name(strip_icon(&tab.name, &self.icon));
            (self.known.get(&tab.id) != Some(&key), tab.id)
        });
        let mut reserved = occupied.clone();
        reserved.extend(self.known.values().cloned());
        reserved.extend(
            tabs.iter()
                .map(|t| sanitize_tab_name(strip_icon(&t.name, &self.icon))),
        );
        let mut used = BTreeSet::new();
        let mut actions = Vec::new();
        for tab in ordered {
            let clean = strip_icon(&tab.name, &self.icon);
            let key = sanitize_tab_name(clean);
            let incoming = self.known.get(&tab.id).is_some_and(|old| old != &key);
            if used.contains(&key) || (incoming && occupied.contains(&key)) {
                // Leave room for the suffix, including with multibyte/long names.
                let mut end = clean.len().min(180);
                while !clean.is_char_boundary(end) {
                    end -= 1;
                }
                let base = clean[..end].trim_end();
                let mut n = 2;
                let name = loop {
                    let candidate = format!("{base} ({n})");
                    if reserved.insert(sanitize_tab_name(&candidate)) {
                        break candidate;
                    }
                    n += 1;
                };
                used.insert(sanitize_tab_name(&name));
                actions.push(Action::RenameTab { id: tab.id, name });
            } else {
                used.insert(key);
            }
        }
        if !actions.is_empty() {
            return actions;
        }

        for tab in tabs {
            let clean = strip_icon(&tab.name, &self.icon).to_string();
            let key = sanitize_tab_name(&clean);

            if let Some(previous) = self.known.get(&tab.id) {
                // Collision resolution above has reserved a distinct destination.
                if previous != &key && notes.contains(previous) && !notes.contains(&key) {
                    notes.remove(previous);
                    notes.insert(key.clone());
                    actions.push(Action::MoveNote {
                        id: tab.id,
                        from: previous.clone(),
                        to: key.clone(),
                    });
                }
            }
            self.known.insert(tab.id, key.clone());

            let expected = decorate(&clean, &self.icon, notes.contains(&key));
            if expected != tab.name {
                actions.push(Action::RenameTab {
                    id: tab.id,
                    name: expected,
                });
            }
        }

        self.known
            .retain(|id, _| tabs.iter().any(|tab| tab.id == *id));
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICON: &str = "📝";

    fn tab(id: usize, name: &str) -> TabView {
        TabView {
            id,
            name: name.to_string(),
        }
    }

    fn notes(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn adds_the_icon_to_a_tab_that_has_a_note() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["dotfiles"]);
        let actions = r.reconcile(&[tab(1, "dotfiles")], &mut n);
        assert_eq!(
            actions,
            vec![Action::RenameTab {
                id: 1,
                name: "📝 dotfiles".to_string()
            }]
        );
    }

    #[test]
    fn leaves_a_tab_without_a_note_alone() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&[]);
        assert_eq!(r.reconcile(&[tab(1, "dotfiles")], &mut n), vec![]);
    }

    #[test]
    fn removes_a_stale_icon_when_the_note_is_gone() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&[]);
        let actions = r.reconcile(&[tab(1, "📝 dotfiles")], &mut n);
        assert_eq!(
            actions,
            vec![Action::RenameTab {
                id: 1,
                name: "dotfiles".to_string()
            }]
        );
    }

    #[test]
    fn is_idempotent() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["dotfiles"]);
        let first = r.reconcile(&[tab(1, "dotfiles")], &mut n);
        assert_eq!(first.len(), 1);
        // Zellij has now applied the rename; the tab comes back decorated.
        let second = r.reconcile(&[tab(1, "📝 dotfiles")], &mut n);
        assert_eq!(second, vec![], "a settled state must produce no actions");
        let third = r.reconcile(&[tab(1, "📝 dotfiles")], &mut n);
        assert_eq!(third, vec![]);
    }

    #[test]
    fn follows_a_rename_by_tab_id() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old"]);
        r.reconcile(&[tab(7, "old")], &mut n);
        let actions = r.reconcile(&[tab(7, "new")], &mut n);
        assert_eq!(
            actions,
            vec![
                Action::MoveNote {
                    id: 7,
                    from: "old".to_string(),
                    to: "new".to_string()
                },
                Action::RenameTab {
                    id: 7,
                    name: "📝 new".to_string()
                },
            ]
        );
        assert_eq!(n, notes(&["new"]), "the note set must be updated in place");
    }

    #[test]
    fn follows_a_rename_that_kept_the_icon_in_the_typed_name() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old"]);
        r.reconcile(&[tab(7, "old")], &mut n);
        let actions = r.reconcile(&[tab(7, "📝 new")], &mut n);
        assert_eq!(
            actions,
            vec![Action::MoveNote {
                id: 7,
                from: "old".to_string(),
                to: "new".to_string()
            }],
            "the name is already correct, only the file moves"
        );
    }

    #[test]
    fn does_not_move_a_note_for_a_tab_that_never_had_one() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&[]);
        r.reconcile(&[tab(7, "old")], &mut n);
        assert_eq!(r.reconcile(&[tab(7, "new")], &mut n), vec![]);
    }

    #[test]
    fn renames_by_tab_id_not_by_display_order() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["b"]);
        let actions = r.reconcile(&[tab(11, "a"), tab(22, "b")], &mut n);
        assert_eq!(
            actions,
            vec![Action::RenameTab {
                id: 22,
                name: "📝 b".to_string()
            }],
            "the action must carry the tab's own id, never its display position"
        );
    }

    #[test]
    fn moving_a_tab_does_not_look_like_a_rename() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["a"]);
        r.reconcile(&[tab(1, "a"), tab(2, "b")], &mut n);
        // MoveTab reorders the tabs but leaves tab ids untouched.
        let actions = r.reconcile(&[tab(2, "b"), tab(1, "📝 a")], &mut n);
        assert_eq!(actions, vec![], "the order changed, the names did not");
    }

    #[test]
    fn forgets_closed_tabs() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["a"]);
        r.reconcile(&[tab(1, "a")], &mut n);
        r.reconcile(&[], &mut n);
        // Tab id 1 is reused by a brand new tab named "b": no note must be moved.
        assert_eq!(r.reconcile(&[tab(1, "b")], &mut n), vec![]);
    }

    #[test]
    fn does_not_move_a_note_onto_another_open_tabs_note() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old", "y"]);
        // Settle with two tabs and two notes
        r.reconcile(&[tab(1, "old"), tab(2, "y")], &mut n);
        // Rename tab 1 from "old" to "y" — destination has a note
        let actions = r.reconcile(&[tab(1, "y"), tab(2, "📝 y")], &mut n);
        // Should not emit MoveNote, but may emit RenameTab
        let has_move_note = actions.iter().any(|a| matches!(a, Action::MoveNote { .. }));
        assert!(
            !has_move_note,
            "must not emit MoveNote when destination has a note"
        );
        // Note set must be unchanged
        assert_eq!(n, notes(&["old", "y"]), "both notes must survive");
    }

    #[test]
    fn does_not_move_a_note_onto_an_orphan_note_file() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old", "z"]);
        // Settle with one tab; "z" has no tab
        r.reconcile(&[tab(1, "old")], &mut n);
        // Rename tab 1 from "old" to "z" — destination has an orphan note
        let actions = r.reconcile(&[tab(1, "z")], &mut n);
        // Should not emit MoveNote
        let has_move_note = actions.iter().any(|a| matches!(a, Action::MoveNote { .. }));
        assert!(
            !has_move_note,
            "must not emit MoveNote when destination has an orphan note"
        );
        // Note set must be unchanged
        assert_eq!(n, notes(&["old", "z"]), "both notes must survive");
    }

    #[test]
    fn decorates_a_tab_whose_name_sanitizes_to_the_note_key() {
        let mut r = Reconciler::new(ICON);
        // The note set comes from filenames, so it holds the sanitized key.
        let mut n = notes(&["feature-login"]);
        let actions = r.reconcile(&[tab(1, "feature/login")], &mut n);
        assert_eq!(
            actions,
            vec![Action::RenameTab {
                id: 1,
                name: "📝 feature/login".to_string()
            }],
            "the icon must follow the note key, the displayed name stays unsanitized"
        );
    }

    #[test]
    fn does_not_move_a_note_onto_a_name_that_shares_a_sanitized_key() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["scratch", "notes-todo"]);
        r.reconcile(&[tab(1, "scratch")], &mut n);
        // "notes/todo" sanitizes to "notes-todo", which already has a note file.
        let actions = r.reconcile(&[tab(1, "notes/todo")], &mut n);
        let has_move_note = actions.iter().any(|a| matches!(a, Action::MoveNote { .. }));
        assert!(
            !has_move_note,
            "the guard must compare keys, not raw display names"
        );
        assert_eq!(
            n,
            notes(&["scratch", "notes-todo"]),
            "both notes must survive"
        );
    }

    #[test]
    fn a_move_carries_note_keys_not_display_names() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old"]);
        r.reconcile(&[tab(7, "old")], &mut n);
        let actions = r.reconcile(&[tab(7, "feature/login")], &mut n);
        assert_eq!(
            actions,
            vec![
                Action::MoveNote {
                    id: 7,
                    from: "old".to_string(),
                    to: "feature-login".to_string()
                },
                Action::RenameTab {
                    id: 7,
                    name: "📝 feature/login".to_string()
                },
            ]
        );
        assert_eq!(n, notes(&["feature-login"]));
    }
    #[test]
    fn duplicate_names_get_distinct_persistent_names_before_reading() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["review"]);
        assert_eq!(
            r.reconcile(&[tab(2, "review"), tab(1, "review")], &mut n),
            vec![Action::RenameTab {
                id: 2,
                name: "review (2)".into()
            }]
        );
        let actions = r.reconcile(&[tab(2, "review (2)"), tab(1, "review")], &mut n);
        assert_eq!(
            actions,
            vec![Action::RenameTab {
                id: 1,
                name: "📝 review".into()
            }]
        );
        assert_eq!(n, notes(&["review"]));
    }

    #[test]
    fn incoming_rename_keeps_its_own_note_and_does_not_steal_owner() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["mine", "review"]);
        r.reconcile(&[tab(1, "mine"), tab(2, "review")], &mut n);
        assert_eq!(
            r.reconcile(&[tab(1, "review"), tab(2, "📝 review")], &mut n),
            vec![Action::RenameTab {
                id: 1,
                name: "review (2)".into()
            }]
        );
        let actions = r.reconcile(&[tab(1, "review (2)"), tab(2, "📝 review")], &mut n);
        assert!(actions.contains(&Action::MoveNote {
            id: 1,
            from: "mine".into(),
            to: "review (2)".into()
        }));
        assert_eq!(n, notes(&["review", "review (2)"]));
    }

    #[test]
    fn sanitization_collisions_are_disambiguated_too() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&[]);
        assert_eq!(
            r.reconcile(&[tab(1, "feature/login"), tab(2, "feature-login")], &mut n),
            vec![Action::RenameTab {
                id: 2,
                name: "feature-login (2)".into()
            }]
        );
    }

    #[test]
    fn suffix_reserves_empty_files_and_other_tabs_names() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old"]);
        r.reconcile(&[tab(1, "old")], &mut n);
        let occupied = notes(&["old", "new", "new (2)"]);
        assert_eq!(
            r.reconcile_with_occupied(&[tab(1, "new"), tab(2, "new (3)")], &mut n, &occupied),
            vec![Action::RenameTab {
                id: 1,
                name: "new (4)".into()
            }]
        );
    }

    #[test]
    fn long_names_leave_room_for_a_suffix_instead_of_looping() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&[]);
        let long = "é".repeat(210);
        let actions = r.reconcile(&[tab(1, &long), tab(2, &long)], &mut n);
        let Action::RenameTab { name, .. } = &actions[0] else {
            panic!()
        };
        assert!(name.ends_with(" (2)"));
        assert!(name.len() < 200);
        assert_ne!(sanitize_tab_name(name), sanitize_tab_name(&long));
    }

    #[test]
    fn failed_move_retries_original_note_under_a_free_name() {
        let mut r = Reconciler::new(ICON);
        let mut n = notes(&["old"]);
        r.reconcile(&[tab(1, "old")], &mut n);
        r.reconcile(&[tab(1, "new")], &mut n);
        r.restore_note_key(1, "old".into());
        n = notes(&["old", "new"]); // concurrent destination creation
        assert_eq!(
            r.reconcile(&[tab(1, "new")], &mut n),
            vec![Action::RenameTab {
                id: 1,
                name: "new (2)".into()
            }]
        );
        let actions = r.reconcile(&[tab(1, "new (2)")], &mut n);
        assert!(actions.contains(&Action::MoveNote {
            id: 1,
            from: "old".into(),
            to: "new (2)".into()
        }));
    }
}
