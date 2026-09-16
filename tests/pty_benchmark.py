#!/usr/bin/env python3
"""Release PTY benchmark. Raw observations include failures; no outlier trimming."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
from pty_smoke import Terminal, REPO, DOWN, RIGHT, LEFT, configure


def frame(terminal):
    terminal.wait(lambda: terminal.screen.frame_count > 0 and "depth 1" in terminal.screen.text() and "Ctrl+A add root" in terminal.screen.text(), "complete initial synchronized frame")


def selected(terminal, suffix, query=None):
    terminal.wait(lambda: (query is None or f"filter: {query} " in terminal.screen.text()) and any(line.startswith("Target:") and line.rstrip().rstrip("/").endswith(suffix.rstrip("/")) for line in terminal.screen.text().splitlines()), "usable target " + suffix)


def stop(terminal):
    terminal.send(b"\x03")
    terminal.finish(130)


def run(binary, samples, output, warm_only=False, max_warm_p95=None):
    observations = {}
    def record(metric, operation):
        started = time.perf_counter_ns()
        try:
            operation()
            result = {"ms": (time.perf_counter_ns() - started) / 1e6, "ok": True}
        except Exception as error:
            result = {"ms": (time.perf_counter_ns() - started) / 1e6, "ok": False, "error": str(error)}
            observations.setdefault(metric, []).append(result)
            raise
        observations.setdefault(metric, []).append(result)

    metadata = {
        "os": platform.platform(), "machine": platform.machine(), "cpu": platform.processor(),
        "terminal": "stdlib PTY, xterm-256color", "size": [120, 30], "samples_requested": samples,
        "binary": str(binary), "binary_bytes": binary.stat().st_size,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "profile": "release", "clock": "perf_counter_ns", "os_disk_cache": "uncontrolled",
        "shell_startup": "user rc files disabled; fixture PATH preserved", "colors": "NO_COLOR removed in test fixture",
        "frame_boundary": "CSI ?2026l synchronized-update completion; no intermediate frame accepted",
        "exact_match_boundary": "complete exact query header plus selected Target path",
        "limits": ["PTY stream and parsed frame timing, not physical display pixels", "Python parser and driver overhead included", "New-process scans are session-cache cold; disk cache is uncontrolled", "No simulated 500 ms I/O, 60-minute soak, RSS sampling, or live Herdr test"],
    }
    try:
        metadata["cpu"] = subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        metadata["cpu_model"] = "unavailable: sysctl restricted; processor field reports architecture only"

    def save():
        summaries = {}
        for name, values in observations.items():
            successful = sorted(value["ms"] for value in values if value["ok"])
            summaries[name] = {"n": len(values), "failures": sum(not v["ok"] for v in values)}
            if successful:
                summaries[name].update(p50_ms=successful[math.ceil(len(successful) * .5) - 1], p95_ms=successful[math.ceil(len(successful) * .95) - 1], max_ms=max(successful))
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps({"metadata": metadata, "summary": summaries, "samples": observations}, indent=2))
        return summaries

    with tempfile.TemporaryDirectory(prefix="qnav-bench-", dir="/private/tmp") as temporary:
        fixture = Path(temporary).resolve()
        roots = [fixture / f"root{i}" for i in range(4)]
        for root in roots:
            for i in range(10):
                branch = root / f"branch{i:02}"
                branch.mkdir(parents=True)
                for j in range(4): (branch / ("needle" if root == roots[0] and i == 0 and j == 0 else f"leaf{j}")).mkdir()
        config = fixture / "config.json"
        configure(config, roots)
        metadata["cold_fixture"] = {"roots": 4, "directories": 200, "depth": 2}
        env = os.environ.copy()
        for name in ("NO_COLOR", "BASH_ENV", "ENV"):
            env.pop(name, None)
        env.update(TERM="xterm-256color", HOME=str(fixture), XDG_CONFIG_HOME=str(fixture / "xdg"), PATH=str(binary.parent) + os.pathsep + env.get("PATH", ""))
        argv = [str(binary), "--config", str(config)]
        try:
            for mode in ([] if warm_only else ["binary", "bash", "zsh"]):
                for _ in range(samples):
                    holder = []
                    def startup():
                        command = argv
                        if mode != "binary":
                            shell = shutil.which(mode)
                            if not shell: raise RuntimeError(mode + " unavailable")
                            command = [shell, "-f", "-c", "source " + shlex.quote(str(REPO / "shell" / ("qnav." + mode))) + "; qnav --config " + shlex.quote(str(config))]
                        terminal = Terminal(command, env, fixture, (120, 30), "benchmark-startup-" + mode)
                        holder.append(terminal)
                        frame(terminal)
                    try:
                        record("startup_" + mode, startup)
                        terminal = holder[0]
                        if mode == "binary":
                            def cold_foreground():
                                terminal.send(DOWN)
                                terminal.contains("branch00")
                                terminal.send(RIGHT + DOWN)
                                selected(terminal, "/branch00")
                            record("foreground_uncached", cold_foreground)
                            terminal.send(LEFT)
                            selected(terminal, str(roots[0]))
                            def warm_foreground():
                                terminal.send(RIGHT)
                                selected(terminal, "/branch00")
                            record("foreground_cached", warm_foreground)
                            stop(terminal)
                            terminal.close()
                            holder.clear()
                            terminal = Terminal(argv, env, fixture, (120, 30), "benchmark-cold-match")
                            holder.append(terminal)
                            frame(terminal)
                            terminal.send(b"]")
                            terminal.contains("depth 2")
                            def cold_match():
                                terminal.send(b"needle")
                                selected(terminal, "/needle", query="needle")
                            record("exact_match_first_200_directories", cold_match)
                        stop(terminal)
                    finally:
                        if holder: holder[0].close()
                print("completed startup " + mode, flush=True)
                save()

            warm_roots = [fixture / f"warm{i:02}" for i in range(20)]
            for root in warm_roots:
                for i in range(25):
                    branch = root / f"branch{i:02}"
                    branch.mkdir(parents=True)
                    for j in range(19): (branch / ("needle" if root == warm_roots[0] and i == 0 and j == 0 else f"leaf{j:02}")).mkdir()
            warm_config = fixture / "warm.json"
            configure(warm_config, warm_roots)
            metadata["warm_fixture"] = {"roots": 20, "directories": 10000, "depth": 2}
            terminal = Terminal([str(binary), "--config", str(warm_config)], env, fixture, (120, 30), "benchmark-warm-10000")
            try:
                frame(terminal)
                terminal.send(b"]needle")
                terminal.wait(lambda: "complete" in terminal.screen.text() and "/needle" in terminal.screen.text(), "warm candidate collection", timeout=60)
                for _ in range(samples):
                    def typing():
                        terminal.send(b"\x7f")
                        selected(terminal, "/needle", query="needl")
                    record("typing_frame_10000_candidates", typing)
                    def warm_exact():
                        terminal.send(b"e")
                        selected(terminal, "/needle", query="needle")
                    record("exact_match_warm_10000_candidates", warm_exact)
                stop(terminal)
            finally: terminal.close()
        finally:
            summaries = save()
    print(json.dumps(summaries, indent=2))
    print("Raw samples: " + str(output))
    if max_warm_p95 is not None:
        measured = summaries["exact_match_warm_10000_candidates"]["p95_ms"]
        assert measured <= max_warm_p95, f"warm exact P95 {measured:.2f} ms exceeds {max_warm_p95:.2f} ms"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/qnav")
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--output", type=Path, default=REPO / "test-artifacts/pty-benchmark.json")
    parser.add_argument("--warm-only", action="store_true")
    parser.add_argument("--max-warm-p95", type=float)
    args = parser.parse_args()
    if args.samples < 1: parser.error("--samples must be positive")
    run(args.binary.resolve(), args.samples, args.output, args.warm_only, args.max_warm_p95)
