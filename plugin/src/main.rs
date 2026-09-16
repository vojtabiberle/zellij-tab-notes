mod fs_ops;
mod modal;
mod watcher;

use modal::Modal;
use std::collections::BTreeMap;
use tab_notes_core::config::Config;
use watcher::Watcher;
use zellij_tile::prelude::*;

enum State {
    Watcher(Watcher),
    Modal(Modal),
}

impl Default for State {
    fn default() -> Self {
        State::Watcher(Watcher::new(Err("tab-notes: not configured".to_string())))
    }
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::OpenFiles,
            PermissionType::RunCommands,
            PermissionType::MessageAndLaunchOtherPlugins,
        ]);
        let parsed = Config::from_map(&configuration);
        if let Err(error) = &parsed {
            eprintln!("{error}");
        }
        *self = match configuration.get("role").map(String::as_str) {
            Some("modal") => State::Modal(Modal::new(parsed)),
            _ => State::Watcher(Watcher::new(parsed)),
        };
        match self {
            State::Watcher(watcher) => watcher.load(),
            State::Modal(modal) => modal.load(),
        }
    }

    fn update(&mut self, event: Event) -> bool {
        match self {
            State::Watcher(watcher) => watcher.update(event),
            State::Modal(modal) => modal.update(event),
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        match self {
            State::Watcher(watcher) => watcher.pipe(pipe_message),
            State::Modal(modal) => modal.pipe(pipe_message),
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if let State::Modal(modal) = self {
            modal.render(rows, cols);
        }
    }
}

register_plugin!(State);
