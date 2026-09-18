#!/usr/bin/env python3
"""Real qnav PTY smoke tests; fixtures and shell settings are isolated.

Run after cargo build: python3 tests/pty_smoke.py [--binary PATH]
No third-party terminal parser or sleeps are required. This checks visible
behavior, not filesystem syscall counts or real Herdr workspace focus.
"""
import argparse
import codecs
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unicodedata

REPO = Path(__file__).resolve().parents[1]
DOWN, RIGHT, LEFT = b"\x1b[B", b"\x1b[C", b"\x1b[D"


class Screen:
    """Small VT projection for Crossterm's cursor-addressed Ratatui output."""
    def __init__(self, width, height):
        self.width, self.height = width, height
        self.cells = [[" "] * width for _ in range(height)]
        self.x = self.y = 0
        self.pending = ""
        self.synchronized = False
        self.frame_count = 0
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            if self.pending.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-9;?]*)([ -/]*)([@-~])", self.pending)
                if not match:
                    return
                raw, _, command = match.groups()
                values = [int(n) if n else 0 for n in raw.lstrip("?").split(";")]
                n = values[0] or 1
                if raw == "?2026" and command in "hl":
                    self.synchronized = command == "h"
                    if command == "l": self.frame_count += 1
                elif command in "Hf":
                    self.y = min(self.height - 1, n - 1)
                    self.x = min(self.width - 1, (values[1] if len(values) > 1 else 1) - 1)
                elif command == "G": self.x = min(self.width - 1, n - 1)
                elif command == "A": self.y = max(0, self.y - n)
                elif command == "B": self.y = min(self.height - 1, self.y + n)
                elif command == "C": self.x = min(self.width - 1, self.x + n)
                elif command == "D": self.x = max(0, self.x - n)
                elif command == "J" and values[0] in (2, 3):
                    self.cells = [[" "] * self.width for _ in range(self.height)]
                elif command == "K":
                    start = 0 if values[0] in (1, 2) else self.x
                    end = self.width if values[0] in (0, 2) else self.x + 1
                    self.cells[self.y][start:end] = [" "] * (end - start)
                self.pending = self.pending[match.end():]
                continue
            if self.pending == "\x1b": return
            char, self.pending = self.pending[0], self.pending[1:]
            if char == "\r": self.x = 0
            elif char == "\n": self.y = min(self.height - 1, self.y + 1)
            elif char == "\b": self.x = max(0, self.x - 1)
            elif char >= " " and char != "\x7f":
                if unicodedata.combining(char):
                    if self.x: self.cells[self.y][self.x - 1] += char
                    continue
                width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
                if self.x < self.width:
                    self.cells[self.y][self.x] = char
                    if width == 2 and self.x + 1 < self.width: self.cells[self.y][self.x + 1] = ""
                self.x = min(self.width, self.x + width)

    def text(self):
        return "\n".join("".join(row) for row in self.cells)


class Terminal:
    def __init__(self, argv, env, cwd, size, label):
        self.label = label
        self.master, self.slave = pty.openpty()
        self.initial = termios.tcgetattr(self.slave)
        width, height = size
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.screen = Screen(width, height)
        self.output = bytearray()
        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(self.slave, termios.TIOCSCTTY, 0)
        # Keep the session leader alive until termios has been inspected. macOS
        # revokes slave ioctls after its controlling session leader exits.
        supervisor = ["/bin/bash", "--noprofile", "--norc", "-c",
                      '"$@"; qnav_pty_status=$?; printf "\\nQNAV_PTY_DONE:%s\\n" "$qnav_pty_status"; IFS= read -r qnav_pty_release; exit "$qnav_pty_status"',
                      "qnav-pty-supervisor", *argv]
        self.process = subprocess.Popen(supervisor, stdin=self.slave, stdout=self.slave,
                                        stderr=self.slave, env=env, cwd=cwd,
                                        preexec_fn=controlling_terminal)

    def pump(self, timeout):
        ready, _, _ = select.select([self.master], [], [], timeout)
        if ready:
            try: data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno == errno.EIO: return
                raise
            self.output.extend(data)
            self.screen.feed(data)

    def wait(self, predicate, step, timeout=5):
        deadline = time.monotonic() + timeout
        while self.screen.synchronized or not predicate():
            if time.monotonic() >= deadline:
                raise AssertionError(f"{self.label}: {step}\n{self.screen.text()}")
            self.pump(min(.05, max(0, deadline - time.monotonic())))

    def contains(self, text, step=None):
        self.wait(lambda: text in self.screen.text(), step or text)

    def send(self, data): os.write(self.master, data)

    def paste(self, text): self.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")

    def finish(self, expected):
        marker = ("QNAV_PTY_DONE:" + str(expected)).encode()
        self.wait(lambda: marker in self.output, "qnav exit status")
        assert termios.tcgetattr(self.slave) == self.initial, f"{self.label}: terminal flags not restored"
        assert b"\x1b[?1049l" in self.output, "alternate screen not restored"
        assert b"\x1b[?2004l" in self.output, "bracketed paste not disabled"
        assert b"\x1b[?25h" in self.output, "cursor not restored"
        for mode in (1000, 1002, 1003, 1015, 1006):
            assert f"\x1b[?{mode}l".encode() in self.output, f"mouse mode {mode} not disabled"
        self.send(b"\n")
        self.wait(lambda: self.process.poll() is not None, "supervisor exit")
        self.pump(0)
        assert self.process.returncode == expected, (self.label, self.process.returncode, bytes(self.output))

    def close(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try: self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait()
        artifacts = REPO / "test-artifacts" / "pty-smoke"
        artifacts.mkdir(parents=True, exist_ok=True)
        (artifacts / (self.label + ".ansi")).write_bytes(self.output)
        (artifacts / (self.label + ".txt")).write_text(self.screen.text())
        os.close(self.master)
        os.close(self.slave)


def configure(path, roots):
    path.write_text(json.dumps({"version": 1, "roots": [{"id": i + 1, "path": str(root)} for i, root in enumerate(roots)]}))


def smoke(binary):
    with tempfile.TemporaryDirectory(prefix="qnav-pty-") as temporary:
        fixture = Path(temporary).resolve()
        root = fixture / "root"
        (root / "alpha" / "nested").mkdir(parents=True)
        (root / "beta").mkdir()
        (root / "bad\x1b[31mname").mkdir()
        config = fixture / "config.json"
        configure(config, [root])
        env = os.environ.copy()
        for name in ("NO_COLOR", "BASH_ENV", "ENV"):
            env.pop(name, None)
        env.update(TERM="xterm-256color", HOME=str(fixture), XDG_CONFIG_HOME=str(fixture / "xdg"), PATH=str(binary.parent) + os.pathsep + env.get("PATH", ""))
        for size in [(120, 30), (80, 24), (40, 12)]:
            terminal = Terminal([str(binary), "--config", str(config)], env, fixture, size, f"navigation-{size[0]}x{size[1]}")
            try:
                terminal.contains("depth 3")
                assert "alpha" not in terminal.screen.text(), "startup unexpectedly shows child listing"
                terminal.contains("(none")
                terminal.send(DOWN)
                terminal.contains("Target:")
                terminal.send(RIGHT)
                terminal.contains("alpha")
                terminal.contains("(none")
                terminal.send(DOWN)
                terminal.contains("> alpha")
                def full_target_or_hint():
                    lines = terminal.screen.text().splitlines()
                    target_lines = []
                    for index, line in enumerate(lines):
                        if line.startswith("Target:"):
                            target_lines = lines[index:]
                            break
                    compact = "".join(line.strip() for line in target_lines)
                    return str(root / "alpha") in compact or "Resize to read full target:" in terminal.screen.text()
                terminal.wait(full_target_or_hint, "full selected target or explicit resize hint")
                for shortcut in ("Ctrl+O hd", "Ctrl+Y branch", "Ctrl+P pane", "Ctrl+C exit", "Ctrl+A add root", "Ctrl+N mkdir", "Ctrl+G git init", "Ctrl+R refresh branch", "Ctrl+L reload roots"):
                    terminal.contains(shortcut, "readable complete shortcut " + shortcut)
                assert b"38;2;245;247;250" in terminal.output, "missing high-contrast help text"
                assert b"48;2;24;29;36" in terminal.output, "missing help background"
                assert b"38;2;128;216;255" in terminal.output, "missing distinct shortcut color"
                capture = REPO / "test-artifacts" / "readability"
                capture.mkdir(parents=True, exist_ok=True)
                (capture / (terminal.label + ".txt")).write_text(terminal.screen.text())
                terminal.send(b"\x03")
                terminal.finish(130)
            finally: terminal.close()
            print(f"PASS {terminal.label}: selection, unselected column, Ctrl-C, terminal restoration")

        terminal = Terminal([str(binary), "--config", str(config)], env, fixture, (120, 30), "filter-refresh-paste")
        try:
            terminal.contains("depth 3")
            terminal.send(DOWN)
            terminal.contains("alpha")
            terminal.contains(r"bad\u{1b}[31mname")
            assert b"bad\x1b[31mname" not in terminal.output, "filename injected terminal controls"
            terminal.send(b"]")
            terminal.contains("depth 4")
            terminal.send(b"nested")
            terminal.contains("/nested")
            terminal.send(b"\t")
            terminal.contains("type to filter")
            terminal.contains("/nested")
            terminal.send(b"beta")
            terminal.contains("filter: beta")
            terminal.send(b"\x1b")
            terminal.contains("type to filter")
            terminal.contains("/nested")
            terminal.send(LEFT + LEFT)
            terminal.contains(str(root))
            (root / "newclone").mkdir()
            terminal.send(b"\x12")
            terminal.contains("newclone")
            terminal.paste("[]")
            terminal.contains("filter: []")
            terminal.contains("depth 4")
            terminal.paste("bad\n\x01")
            terminal.contains("Paste rejected")
            terminal.contains("filter: []")
            assert "Add existing directory" not in terminal.screen.text()
            terminal.send(b"\x03")
            terminal.finish(130)
        finally: terminal.close()
        print("PASS filter-refresh-paste: depth, unvisited match, Tab, Esc, external clone refresh, paste and escaping")

        added = fixture / "added []"
        added.mkdir()
        empty_config = fixture / "new-config.json"
        terminal = Terminal([str(binary), "--config", str(empty_config)], env, fixture, (80, 24), "add-root-paste")
        try:
            terminal.contains("Add a directory")
            terminal.send(b"\x01")
            terminal.contains("Add existing directory")
            terminal.paste(str(added))
            terminal.contains("added []")
            assert not empty_config.exists(), "paste submitted form"
            terminal.send(b"\r")
            terminal.contains("Root added:")
            assert json.loads(empty_config.read_text())["roots"][0]["path"] == str(added)
            terminal.contains("depth 3")
            terminal.send(b"\x03")
            terminal.finish(130)
        finally: terminal.close()
        print("PASS add-root-paste: explicit submit, saved native path, brackets preserve depth")

        for shell in ["bash", "zsh"]:
            executable = shutil.which(shell)
            assert executable, f"required shell unavailable: {shell}"
            command = "source " + shlex.quote(str(REPO / "shell" / ("qnav." + shell))) + "; qnav --config " + shlex.quote(str(config)) + '; qnav_status=$?; printf "\\nQNAV_RC:%s\\n" "$qnav_status"; printf "QNAV_CWD:%s\\n" "$PWD"'
            terminal = Terminal([executable, "-f", "-c", command], env, fixture, (120, 30), "wrapper-" + shell)
            try:
                terminal.contains("depth 3")
                terminal.send(DOWN)
                terminal.contains("alpha")
                terminal.send(RIGHT)
                terminal.contains("(none")
                terminal.send(DOWN)
                terminal.contains("/alpha")
                terminal.send(b"\r")
                terminal.finish(0)
                assert b"QNAV_RC:0" in terminal.output
                assert ("QNAV_CWD:" + str(root / "alpha")).encode() in terminal.output
            finally: terminal.close()
            print(f"PASS wrapper-{shell}: Enter changes real parent shell cwd to selected child")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/qnav")
    arguments = parser.parse_args()
    smoke(arguments.binary.resolve())
