/// A fixed script: directory names are positional arguments, never shell source.
/// Merge only regular notes and never overwrite a destination. Keep the source
/// directory (and any collisions) so an editor still using its old path can save.
pub const MIGRATE_NOTES: &str = r#"
set -eu
[ "$1" != "$2" ] || exit 0
[ ! -L "$1" ] && [ ! -L "$2" ] || exit 1
mkdir -p "$2"
[ -d "$1" ] || exit 0
for note in "$1"/*.md; do
    [ -f "$note" ] && [ ! -L "$note" ] || continue
    target="$2/${note##*/}"
    [ ! -e "$target" ] && [ ! -L "$target" ] || continue
    mv -n "$note" "$2/"
done
"#;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Notes(PathBuf);
    impl Notes {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "tab-notes-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn migrate(&self, from: &str, to: &str) -> bool {
            Command::new("sh")
                .current_dir(&self.0)
                .args(["-c", MIGRATE_NOTES, "tab-notes-migrate"])
                .arg(self.0.join(from))
                .arg(self.0.join(to))
                .status()
                .unwrap()
                .success()
        }
    }
    impl Drop for Notes {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn follows_session_renames_and_keeps_old_directory_for_open_editors() {
        let n = Notes::new();
        fs::create_dir(n.0.join("scratch")).unwrap();
        fs::write(
            n.0.join("scratch/review.md"),
            "https://github.com/example/repo/pull/1",
        )
        .unwrap();
        assert!(n.migrate("scratch", "task"));
        assert!(n.migrate("task", "renamed again"));
        assert_eq!(
            fs::read_to_string(n.0.join("renamed again/review.md")).unwrap(),
            "https://github.com/example/repo/pull/1"
        );
        assert!(n.0.join("scratch").is_dir());
        assert!(!n.0.join("scratch/review.md").exists());
    }

    #[test]
    fn merges_without_overwriting_or_deleting_collisions() {
        let n = Notes::new();
        for dir in ["old", "new"] {
            fs::create_dir(n.0.join(dir)).unwrap();
        }
        fs::write(n.0.join("old/shared.md"), "source").unwrap();
        fs::write(n.0.join("new/shared.md"), "destination").unwrap();
        fs::write(n.0.join("old/unique.md"), "keep").unwrap();
        fs::write(n.0.join("old/empty.md"), "").unwrap();
        assert!(n.migrate("old", "new"));
        assert_eq!(
            fs::read_to_string(n.0.join("old/shared.md")).unwrap(),
            "source"
        );
        assert_eq!(
            fs::read_to_string(n.0.join("new/shared.md")).unwrap(),
            "destination"
        );
        assert!(n.0.join("new/unique.md").is_file());
        assert!(n.0.join("new/empty.md").is_file());
    }

    #[test]
    fn names_are_data_not_shell_code_and_same_directory_is_a_noop() {
        let n = Notes::new();
        let from = "scratch ' $(touch PWNED); `id`";
        let to = "task \" $HOME";
        fs::create_dir(n.0.join(from)).unwrap();
        fs::write(n.0.join(from).join("$(touch PWNED).md"), "note").unwrap();
        assert!(n.migrate(from, to));
        assert!(n.migrate(to, to));
        assert_eq!(
            fs::read_to_string(n.0.join(to).join("$(touch PWNED).md")).unwrap(),
            "note"
        );
        assert!(!n.0.join("PWNED").exists());
    }

    #[test]
    fn missing_source_is_normal_and_symlink_directory_is_rejected() {
        let n = Notes::new();
        assert!(n.migrate("missing", "new"));
        std::os::unix::fs::symlink(n.0.join("new"), n.0.join("link")).unwrap();
        assert!(!n.migrate("link", "other"));
        assert!(!n.migrate("missing", "link"));
    }

    #[test]
    fn does_not_follow_note_symlinks() {
        let n = Notes::new();
        for dir in ["old", "new"] {
            fs::create_dir(n.0.join(dir)).unwrap();
        }
        fs::write(n.0.join("outside"), "untouched").unwrap();
        std::os::unix::fs::symlink(n.0.join("outside"), n.0.join("old/link.md")).unwrap();
        fs::write(n.0.join("old/shared.md"), "keep").unwrap();
        std::os::unix::fs::symlink(n.0.join("absent"), n.0.join("new/shared.md")).unwrap();
        assert!(n.migrate("old", "new"));
        assert!(n.0.join("old/shared.md").is_file());
        assert!(n.0.join("old/link.md").is_symlink());
        assert_eq!(
            fs::read_to_string(n.0.join("outside")).unwrap(),
            "untouched"
        );
    }
}
