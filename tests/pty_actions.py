#!/usr/bin/env python3
"""Terminal action wiring: real isolated mkdir/Git, fake hd/Herdr executables."""
import argparse
import os
from pathlib import Path
import shlex
import subprocess
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
            (project / "README").write_text("branch fixture\n")
            subprocess.run(["git", "-C", str(project), "add", "README"], check=True, env=env)
            subprocess.run([
                "git", "-C", str(project), "-c", "user.name=Test", "-c",
                "user.email=test@example.com", "commit", "-qm", "initial",
            ], check=True, env=env)
            subprocess.run(["git", "-C", str(project), "branch", "branch-workspace"], check=True, env=env)
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
            before_cancel = (fixture / "hd.record").read_bytes()
            terminal.send(b"\x0c")
            terminal.contains(" Branch workspace ")
            terminal.contains("new worktree")
            terminal.send(b"\x1b")
            terminal.wait(lambda: " Branch workspace " not in terminal.screen.text(), "branch picker Esc closes")
            assert (fixture / "hd.record").read_bytes() == before_cancel
            terminal.send(b"\x0c")
            terminal.contains(" Branch workspace ")
            terminal.contains("new worktree")
            terminal.paste("branch-workspace")
            terminal.contains("branch-workspace")
            terminal.send(b"\r")
            terminal.wait(lambda: (fixture / "hd.record").read_bytes().split(b"\0")[1:3] == [b"new", b"branch-workspace"] and "Action completed" in terminal.screen.text(), "branch selection dispatches hd new")
            assert " Branch workspace " not in terminal.screen.text()

            terminal.send(b"\x0c")
            terminal.contains("Ctrl+D delete merged")
            terminal.contains("Ctrl+X remove workspace")
            terminal.contains("new worktree")
            terminal.paste("branch-workspace")
            terminal.send(b"\x04")
            terminal.contains("Delete merged branch")
            terminal.contains("[x] Cancel")
            terminal.send(b"\r")
            terminal.wait(lambda: "Delete merged branch" not in terminal.screen.text(), "default branch deletion cancels")
            subprocess.run(["git", "-C", str(project), "show-ref", "--verify", "--quiet", "refs/heads/branch-workspace"], check=True, env=env)
            terminal.send(b"\x04")
            terminal.contains("[x] Cancel")
            terminal.send(RIGHT + b"\r")
            terminal.contains("No matching branches")
            assert subprocess.run(["git", "-C", str(project), "show-ref", "--verify", "--quiet", "refs/heads/branch-workspace"], env=env).returncode != 0
            assert " Branch workspace " in terminal.screen.text()
            terminal.send(b"\x1b")
            terminal.wait(lambda: " Branch workspace " not in terminal.screen.text(), "close refreshed branch list")

            workspace = fixture / "linked-workspace"
            subprocess.run(["git", "-C", str(project), "worktree", "add", "-qb", "workspace-remove", str(workspace)], check=True, env=env)
            (commands / "hd").write_text(
                "#!/bin/sh\nprintf '%s\\0' \"$PWD\" \"$@\" > \"$QNAV_TEST_RECORD/hd.record\"\n"
                "if [ \"$1\" = rm ]; then git worktree remove -- " + shlex.quote(str(workspace)) + "; fi\n"
            )
            before_remove = (fixture / "hd.record").read_bytes()
            terminal.send(b"\x0c")
            terminal.contains(" Branch workspace ")
            terminal.contains(str(workspace))
            terminal.paste("workspace-remove")
            terminal.send(b"\x04")
            terminal.contains("remove its workspace first")
            terminal.wait(lambda: "Loading branches" not in terminal.screen.text(), "branch with workspace is refused and refreshed")
            assert workspace.is_dir()
            assert (fixture / "hd.record").read_bytes() == before_remove
            subprocess.run(["git", "-C", str(project), "show-ref", "--verify", "--quiet", "refs/heads/workspace-remove"], check=True, env=env)
            terminal.send(b"\x18")
            terminal.contains("Remove branch workspace")
            terminal.contains("[x] Cancel")
            terminal.send(b"\r")
            terminal.wait(lambda: "Remove branch workspace" not in terminal.screen.text(), "default workspace removal cancels")
            assert workspace.is_dir()
            assert (fixture / "hd.record").read_bytes() == before_remove
            terminal.send(b"\x18")
            terminal.contains("[x] Cancel")
            terminal.send(RIGHT + b"\r")
            terminal.wait(lambda: not workspace.exists() and "new worktree" in terminal.screen.text(), "workspace removal refreshes branch state")
            assert (fixture / "hd.record").read_bytes().split(b"\0") == [os.fsencode(project), b"rm", b"workspace-remove", b""]
            subprocess.run(["git", "-C", str(project), "show-ref", "--verify", "--quiet", "refs/heads/workspace-remove"], check=True, env=env)
            terminal.send(b"\x04")
            terminal.contains("Delete merged branch")
            terminal.send(RIGHT + b"\r")
            terminal.contains("No matching branches")
            assert subprocess.run(["git", "-C", str(project), "show-ref", "--verify", "--quiet", "refs/heads/workspace-remove"], env=env).returncode != 0
            terminal.send(b"\x1b")
            terminal.wait(lambda: " Branch workspace " not in terminal.screen.text(), "close cleanup picker")

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
            print("PASS PTY actions: mkdir, Git init confirmation, hd/pane exact cwd/argv, merged branch deletion and hd rm default-cancel/confirm/refresh, preserved branch, Ctrl-C child reap and terminal cleanup")
        finally:
            terminal.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/qnav")
    run(parser.parse_args().binary.resolve())
