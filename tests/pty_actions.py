#!/usr/bin/env python3
"""Terminal action wiring: real isolated mkdir/Git, fake hd/Herdr executables."""
import argparse
import os
from pathlib import Path
import sys
import tempfile

sys.dont_write_bytecode = True
from pty_smoke import Terminal, REPO, DOWN, RIGHT, configure


def run(binary):
    with tempfile.TemporaryDirectory(prefix="qnav-actions-", dir="/private/tmp") as temporary:
        fixture = Path(temporary).resolve()
        root, commands = fixture / "root", fixture / "bin"
        root.mkdir()
        commands.mkdir()
        for name in ("hd", "herdr"):
            script = commands / name
            script.write_text("#!/bin/sh\nprintf '%s\\0' \"$PWD\" \"$@\" > \"$QNAV_TEST_RECORD/" + name + ".record\"\n")
            script.chmod(0o700)
        config = fixture / "config.json"
        configure(config, [root])
        env = os.environ.copy()
        # Git/hd tests never inherit repository-bound state from the test driver.
        for key in list(env):
            if key.startswith("GIT_"):
                del env[key]
        env.update(TERM="xterm-256color", QNAV_TEST_RECORD=str(fixture),
                   GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_SYSTEM="/dev/null",
                   PATH=str(commands) + os.pathsep + env.get("PATH", ""))
        terminal = Terminal([str(binary), "--config", str(config)], env, fixture,
                            (120, 30), "actions-mkdir-git-hd-pane")
        try:
            terminal.contains("depth 3")
            terminal.send(DOWN + b"\x0e")
            terminal.contains("Create child directory")
            terminal.paste("new-project")
            terminal.send(b"\r")
            project = root / "new-project"
            terminal.wait(lambda: project.is_dir() and "Target: " + str(project) in terminal.screen.text(), "mkdir selects verified child")
            terminal.send(b"\x07")
            terminal.contains("[x] Cancel")
            terminal.send(b"\r")
            terminal.wait(lambda: "[x] Cancel" not in terminal.screen.text(), "default Git confirmation cancels")
            assert not (project / ".git").exists()
            terminal.send(b"\x07")
            terminal.contains("[x] Cancel")
            terminal.send(RIGHT + b"\r")
            terminal.wait(lambda: (project / ".git").is_dir() and "Action completed" in terminal.screen.text(), "confirmed real git init")
            for key, name in ((b"\x0f", "hd"), (b"\x10", "herdr")):
                terminal.send(key)
                terminal.wait(lambda: (fixture / (name + ".record")).exists() and "Action completed" in terminal.screen.text(), "retained " + name)
                fields = (fixture / (name + ".record")).read_bytes().split(b"\0")
                assert fields[0] == os.fsencode(project)
                if name == "hd":
                    assert fields == [os.fsencode(project), b""]
                else:
                    assert fields[1:-1] == [b"pane", b"split", b"--current", b"--direction", b"right", b"--cwd", os.fsencode(project), b"--focus"]
                assert terminal.process.poll() is None
                assert "Target: " + str(project) in terminal.screen.text()
            (commands / "hd").write_text("#!/bin/sh\nprintf '%s' \"$$\" > \"$QNAV_TEST_RECORD/child.pid\"\ntrap '' TERM\nwhile :; do sleep 10; done\n")
            terminal.send(b"\x0f")
            terminal.wait(lambda: (fixture / "child.pid").exists(), "owned blocking hd started")
            child_pid = int((fixture / "child.pid").read_text())
            terminal.send(b"\x03")
            terminal.finish(130)
            try:
                os.kill(child_pid, 0)
            except ProcessLookupError:
                pass
            else:
                raise AssertionError("qnav exited before reaping its hd child")
            print("PASS PTY actions: mkdir, cancel/confirm real git init, retained mocked hd/pane exact cwd/argv, Ctrl-C child reap and terminal cleanup")
        finally:
            terminal.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/qnav")
    run(parser.parse_args().binary.resolve())
