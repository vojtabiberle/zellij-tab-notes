#!/usr/bin/env python3
"""Live regression test: python3 tests/session-rename.py path/to/tab-notes.wasm.

Requires Zellij 0.45+ and Unix. Uses only temporary notes, configuration, permission
cache and sockets; never attaches to or modifies an existing user session.
"""
import fcntl
import os
from pathlib import Path
import pty
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

wasm = Path(sys.argv[1]).resolve(strict=True)
with tempfile.TemporaryDirectory(prefix="tab-notes-test-") as temporary:
    root = Path(temporary)
    env = {k: v for k, v in os.environ.items() if not k.startswith("ZELLIJ")}
    env.update(TERM="xterm-256color", COLORTERM="truecolor")
    for key, directory in [("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"),
                           ("XDG_DATA_HOME", "data"), ("ZELLIJ_SOCKET_DIR", "sockets")]:
        env[key] = str(root / directory)
        (root / directory).mkdir()
    notes = root / "notes"
    (notes / "scratch").mkdir(parents=True)
    (notes / "scratch" / "review.md").write_text("REVIEW_CONTEXT_8444\n")
    (notes / "target").mkdir()
    (notes / "scratch" / "collision.md").write_text("source\n")
    (notes / "target" / "collision.md").write_text("destination\n")
    url = f"file:{wasm}"
    config = root / "config.kdl"
    config.write_text(f'''default_shell "/bin/sh"
plugins {{
    tab-notes location="{url}" {{
        role "modal"
        notes_dir "{notes}"
    }}
    tab-notes-watcher location="{url}" {{
        role "watcher"
        notes_dir "{notes}"
    }}
}}
load_plugins {{ tab-notes-watcher; }}
''')
    session = "scratch"
    client = None
    master = None
    screen = bytearray()

    def run(*args):
        result = subprocess.run(["zellij", "--config", str(config), *args], env=env,
                                capture_output=True, text=True, timeout=15)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return result.stdout

    def action(*args):
        return run("--session", session, "action", *args)

    def wait_for(predicate, description):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.1)
        raise AssertionError(f"Timed out: {description}; screen tail: {bytes(screen[-2500:])!r}")

    def drain():
        try:
            while True:
                screen.extend(os.read(master, 65536))
        except OSError:
            pass

    try:
        run("attach", "--create-background", session)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 130, 0, 0))
        client = subprocess.Popen(["zellij", "--config", str(config), "attach", session],
                                  env=env, stdin=slave, stdout=slave, stderr=slave,
                                  start_new_session=True)
        os.close(slave)
        threading.Thread(target=drain, daemon=True).start()
        wait_for(lambda: b"Allow?" in screen, "test plugin permission prompt")
        os.write(master, b"y")
        action("rename-tab", "review")
        wait_for(lambda: "📝" in action("list-tabs", "--json"), "initial note marker")
        action("launch-plugin", "--floating", "--configuration", f"role=modal,notes_dir={notes}", url)
        wait_for(lambda: b"REVIEW_CONTEXT_8444" in screen, "initial modal content")
        screen.clear()
        action("rename-session", "target")
        session = "target"
        wait_for(lambda: b"session notes conflict" in screen, "session collision reported")
        assert (notes / "scratch" / "review.md").read_text() == "REVIEW_CONTEXT_8444\n"
        assert (notes / "scratch" / "collision.md").read_text() == "source\n"
        assert not (notes / "target" / "review.md").exists()
        screen.clear()
        action("rename-session", "review-task")
        session = "review-task"
        wait_for(lambda: (notes / session / "review.md").exists(), "note migrated")
        wait_for(lambda: b"REVIEW_CONTEXT_8444" in screen, "open modal refreshed")
        assert (notes / session / "review.md").read_text() == "REVIEW_CONTEXT_8444\n"
        assert not (notes / "scratch" / "review.md").exists()
        assert (notes / session / "collision.md").read_text() == "source\n"
        assert (notes / "target" / "collision.md").read_text() == "destination\n"
        assert "📝" in action("list-tabs", "--json")
        # Consecutive renames exercise the watcher's asynchronous migration queue.
        for name in ("intermediate", "final"):
            action("rename-session", name)
            session = name
        wait_for(lambda: (notes / "final" / "review.md").exists(), "consecutive renames")
        assert (notes / "final" / "review.md").read_text() == "REVIEW_CONTEXT_8444\n"
        # Same display name gets a persistent suffix, never the first tab's note.
        action("new-tab", "--name", "review")
        wait_for(lambda: "review (2)" in action("list-tabs", "--json"), "duplicate tab renamed")
        (notes / "final" / "review (2).md").write_text("SECOND_TAB_CONTEXT\n")
        action("pipe", "--name", "tab-notes:notes-changed", "refresh")
        screen.clear()
        action("launch-plugin", "--floating", "--configuration", f"role=modal,notes_dir={notes}", url)
        wait_for(lambda: b"SECOND_TAB_CONTEXT" in screen, "duplicate tab owns a separate note")
        assert b"REVIEW_CONTEXT_8444" not in screen
        third_id = action("new-tab", "--name", "other").strip()
        (notes / "final" / "other.md").write_text("THIRD_TAB_CONTEXT\n")
        action("pipe", "--name", "tab-notes:notes-changed", "refresh")
        wait_for(lambda: "📝 other" in action("list-tabs", "--json"), "third note indexed")
        action("rename-tab-by-id", third_id, "review")
        wait_for(lambda: (notes / "final" / "review (3).md").exists(), "incoming rename disambiguated")
        assert (notes / "final" / "review (3).md").read_text() == "THIRD_TAB_CONTEXT\n"
        assert (notes / "final" / "review (2).md").read_text() == "SECOND_TAB_CONTEXT\n"
        assert (notes / "final" / "review.md").read_text() == "REVIEW_CONTEXT_8444\n"
        action("new-tab", "--name", "feature/login")
        action("new-tab", "--name", "feature-login")
        wait_for(lambda: "feature-login (2)" in action("list-tabs", "--json"), "sanitized collision")
        print("PASS: session conflict refusal/recovery, modal refresh, duplicate tabs, incoming rename, sanitized names")
    finally:
        for name in ("scratch", "target", "review-task", "intermediate", "final"):
            subprocess.run(["zellij", "delete-session", "--force", name], env=env,
                           capture_output=True, timeout=10)
        if client is not None:
            client.wait(timeout=10)
        if master is not None:
            os.close(master)
