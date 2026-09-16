#!/usr/bin/env python3
"""Mouse interaction against the real terminal. Run after cargo build."""
import argparse
import fcntl
import os
from pathlib import Path
import signal
import struct
import sys
import tempfile
import termios

sys.dont_write_bytecode = True
from pty_smoke import REPO, Screen, Terminal, configure


def point(terminal, label, *, matches=False):
    """Locate a rendered row, excluding Target/footer and column headers."""
    for y, cells in enumerate(terminal.screen.cells):
        line = "".join(cells)
        if "│" not in line or label not in line:
            continue
        x = line.index(label)
        if matches and " / " not in line:
            continue
        return x + 1, y + 1  # SGR terminal coordinates are one-based.
    raise AssertionError(f"Missing row {label!r}\n{terminal.screen.text()}")


def mouse(terminal, position, button=0, *, redraw=True):
    x, y = position
    frame = terminal.screen.frame_count
    terminal.send(f"\x1b[<{button};{x};{y}M".encode())
    if redraw:
        terminal.wait(lambda: terminal.screen.frame_count > frame, "mouse event rendered")
    if button == 0:
        terminal.send(f"\x1b[<0;{x};{y}m".encode())


def selected(terminal, label):
    terminal.contains("> " + label, "selected " + label)


def smoke(binary):
    with tempfile.TemporaryDirectory(prefix="qnav-mouse-", dir="/tmp") as temporary:
        fixture = Path(temporary).resolve()
        root = fixture / "root"
        for index in range(40):
            (root / f"item{index:02}").mkdir(parents=True)
        other = fixture / "other"
        other.mkdir()
        config = fixture / "config.json"
        configure(config, [root, other])
        env = os.environ.copy()
        for name in ("NO_COLOR", "BASH_ENV", "ENV"):
            env.pop(name, None)
        env.update(TERM="xterm-256color", HOME=str(fixture), XDG_CONFIG_HOME=str(fixture / "xdg"))
        result = fixture / "result"
        result.touch(mode=0o600)
        for exit_with_enter in (False, True):
            label = "mouse-enter" if exit_with_enter else "mouse-cancel"
            terminal = Terminal([str(binary), "--config", str(config), "--result-file", str(result)],
                                env, fixture, (120, 30), label)
            try:
                terminal.contains("depth 3")
                for mode in (1000, 1002, 1003, 1015, 1006):
                    assert f"\x1b[?{mode}h".encode() in terminal.output, f"mouse mode {mode} not enabled"
                mouse(terminal, point(terminal, "root"))
                terminal.contains("item00")
                # The child column is visible but unfocused; a click must focus it.
                mouse(terminal, point(terminal, "item03"))
                selected(terminal, "item03")
                mouse(terminal, point(terminal, "root"), 65)
                selected(terminal, "other")
                mouse(terminal, point(terminal, "root"), 64)
                selected(terminal, "root")
                terminal.contains("item00")
                mouse(terminal, point(terminal, "item03"))
                # Wheel navigation must use the hovered child column.
                mouse(terminal, point(terminal, "item03"), 65)
                selected(terminal, "item04")
                for index in range(4, 26):
                    mouse(terminal, point(terminal, f"item{index:02}"), 65)
                selected(terminal, "item26")
                # Select a visible row after scrolling; raw row number is not its index.
                mouse(terminal, point(terminal, "item25"))
                selected(terminal, "item25")
                terminal.send(b"\x0e")
                terminal.contains("Create child directory")
                mouse(terminal, point(terminal, "root"), redraw=False)
                terminal.send(b"\x1b")
                selected(terminal, "item25")
                terminal.send(b"item")
                terminal.contains("Matches 40")
                mouse(terminal, point(terminal, "item03", matches=True))
                selected(terminal, str(root) + " / item03")
                mouse(terminal, point(terminal, "item03", matches=True), 65)
                selected(terminal, str(root) + " / item04")
                terminal.send(b"\t")
                selected(terminal, "item04")
                # Force Ratatui to clear and redraw at the new terminal geometry.
                terminal.screen = Screen(80, 24)
                fcntl.ioctl(terminal.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
                os.killpg(terminal.process.pid, signal.SIGWINCH)
                terminal.contains("item04")
                mouse(terminal, point(terminal, "item02"))
                selected(terminal, "item02")
                terminal.send(b"\r" if exit_with_enter else b"\x03")
                terminal.finish(0 if exit_with_enter else 130)
                if exit_with_enter:
                    assert result.read_bytes() == b"qnav-v1\0cd\0" + os.fsencode(root / "item02") + b"\0"
            finally:
                terminal.close()
            print(f"PASS {label}: click, hovered wheel, scroll offset, modal, filter, resize, terminal restoration")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/debug/qnav")
    smoke(parser.parse_args().binary.resolve())
