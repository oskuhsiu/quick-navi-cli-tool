# qnav

A directory-only terminal navigator with Finder-style columns. Pin the places
you work in, browse with the arrow keys, or type to jump between projects.

qnav starts with **only the roots you add**. It explores a bounded depth, skips
symlinks and common cache/build directories, and keeps its index in memory.
There is no background daemon, whole-disk scan, or on-disk search database.

## Install

Currently tested on macOS. Building requires Rust 1.88+ and Cargo; changing the
calling shell's directory requires Bash or Zsh. Git, `hd`, and Herdr are optional
and needed only for their respective actions.

```sh
git clone https://github.com/oskuhsiu/quick-navi-cli-tool.git
cd quick-navi-cli-tool
cargo install --path . --locked
```

Cargo normally installs the binary to `~/.cargo/bin`. Make sure that directory
is on your `PATH`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

### Enable Enter to change directories

**Installation alone is not shell setup.** A program cannot change its parent
shell's working directory, so qnav provides a small shell function that performs
the final `cd` after qnav exits.

From the cloned repository, load **one** wrapper matching your current shell:

```sh
# Zsh
source shell/qnav.zsh
qnav
```

```sh
# Bash
source shell/qnav.bash
qnav
```

This affects only the current shell. To enable it in new sessions, add the PATH
line and a `source` line with the wrapper's **absolute path** to your shell's
startup file (for example, `~/.zshrc` or the file your Bash setup loads):

```sh
export PATH="$HOME/.cargo/bin:$PATH"
source /absolute/path/to/quick-navi-cli-tool/shell/qnav.zsh
```

Use `qnav.bash` for Bash. Keep the checkout at that location, or copy the matching
wrapper to a stable location and source that instead. qnav does not edit startup
files or create aliases. The command is `qnav`; a shorter alias is optional.

Without the wrapper, browsing and other actions still work, but Enter cannot
change your shell directory. If you see **Enter needs shell setup**, leave with
Ctrl+C, load the wrapper, and run `qnav` again. `command qnav --help` also explains
the setup.

## First run

1. Run `qnav`. The first column is empty until you add a root.
2. Press Ctrl+A, enter a directory such as `~/Projects`, then press Enter.
3. Use ↑/↓ to select that root. Its immediate directories appear in the next
   column; use → and ↑/↓ to select a project.
4. Press Enter to exit and `cd` there, or Ctrl+O to open it with your external
   `hd` while leaving qnav running.

For search, just start typing a directory name. Results come from all your roots
within the allowed depth, including branches you have not browsed. Use ↑/↓ to
choose a result, Enter to `cd`, or Tab to continue browsing from that result.
Esc clears the filter and restores your previous browse position.

The **Target** line always shows the directory an action will use. A newly
revealed column has no selected row until you choose one; actions do not silently
fall back to another column's selection.

## Keyboard reference

| Key | Action |
| --- | --- |
| ↑ / ↓ | Select a directory in the current column or a search result |
| → / ← | Move to the next column / return to its parent |
| `[` / `]` | Decrease / increase exploration depth: 1–9, default 1 |
| Type | Filter directory names across all roots within the allowed depth |
| Tab | Adopt the selected search result and continue column browsing |
| Esc | Clear the filter and restore browsing, or cancel an input/confirmation |
| Enter | Exit qnav and change the calling shell's directory; requires wrapper |
| Ctrl+O | Run external `hd` at the target; keep qnav open |
| Ctrl+P | Open a Herdr shell pane at the target; keep qnav open |
| Ctrl+A | Add an existing directory as a root |
| Ctrl+N | Create one child directory under the target |
| Ctrl+G | Confirm and run `git init` at the target |
| Ctrl+R | Refresh the branch displayed in the current column |
| Ctrl+L | Reload roots from configuration and invalidate all cached listings |
| Ctrl+C | Exit without changing the calling shell's directory |

Git initialization defaults to **Cancel**; use an arrow to select Continue,
then Enter. Ctrl+C is the exit shortcut; double-Esc is not required.

The shortcut bar uses bold, high-contrast text and wraps complete key/description
pairs in narrow panes. Font size is controlled by your terminal, not qnav.

## Exploration and refresh

A pinned root is depth **0**; its immediate directories are depth **1**. The
default depth is 1 and resets when qnav starts. `[` and `]` change how far qnav may
explore, not how many columns it displays at once.

In normal browsing, children are read as you select their parents. Filtering
explores all roots up to the depth limit. To keep project navigation focused,
pin directories that directly contain your projects and increase depth only
when needed.

There is no filesystem watcher. If another program adds, removes, or renames a
directory:

- **Ctrl+R** refreshes the current column's parent branch, including new siblings.
  In the root column, it refreshes the selected root.
- **Ctrl+L** reloads configuration and clears cached listings for all roots;
  needed listings are then read again. It does not immediately crawl everything.

Neither refresh increases the exploration depth. Loading, permission errors,
and incomplete results are shown explicitly. Exploration is bounded by 50,000
cached child occurrences and 1,000,000 inspected entries per exploration budget.

### What is skipped

Only directories are listed. Symlinks are neither shown nor followed during
exploration. Hidden directories such as `.codex` and `.local` remain available.

These child directory names are excluded:

```text
.git  .cache  node_modules  .venv  venv  .gradle
.next  build  dist  target  vendor
```

The `/Library`, `/Applications`, `/System/Library`, `~/Library`, and
`~/Applications` trees are also excluded unless explicitly pinned as roots.
Explicitly pinning a normally excluded directory allows that root to be explored;
the usual child-name exclusions still apply within it.

Exclusions are currently built in, not configurable. qnav does not read
`.gitignore` to decide what to explore.

## Configuration and storage

Roots are saved after a successful Add operation:

- Default: `~/.config/qnav/config.json`.
- With `XDG_CONFIG_HOME` set: `$XDG_CONFIG_HOME/qnav/config.json`. Use an absolute
  path for `XDG_CONFIG_HOME`.
- Override for a separate root list: `qnav --config /absolute/path/config.json`.

A small `config.lock` file beside the configuration coordinates concurrent
saves. Invalid configuration is reported and preserved, not overwritten.

There is **no persistent directory cache**. Listings, search queries, selection,
and exploration depth stay in memory and are discarded when qnav exits.

Example configuration:

```json
{
  "version": 1,
  "roots": [
    { "id": 1, "path": "/absolute/physical/projects" },
    { "id": 2, "path": "/absolute/physical/documents" }
  ]
}
```

To remove or reorder roots, edit this file and press Ctrl+L. IDs must be unique
positive integers; paths must be unique, normalized absolute physical paths.
Using Ctrl+A is the easiest way to create valid entries.

Ctrl+A accepts absolute paths, paths relative to qnav's startup directory, and
`~/…`. It does not expand environment variables, globs, or shell expressions.
Pasted text is data: pasted brackets do not change depth, and pasted newlines do
not submit a form.

## Optional integrations and limitations

`hd` and `herdr` must be executable commands on `PATH`, not aliases or shell
functions. Plain browsing and Enter do not require either tool.

- **Ctrl+O:** the target must be inside a Git repository. qnav runs your existing
  `hd` there; workspace behavior belongs to that tool.
- **Ctrl+P:** requires Herdr and an active Herdr pane. It creates and focuses a
  right-hand shell pane with the exact target directory as its working directory.
- **Ctrl+G:** requires Git and explicit confirmation. qnav does not create
  commits, configure remotes, or push your projects.

Known compatibility issue observed with **hd 0.4.0 + Herdr 0.9.0**: reopening a
primary repository can create another workspace because that hd version expects
a `cwd` field missing from Herdr's workspace listing. qnav does not patch or
replace either tool.

Git/hd actions reject repository-binding Git environment variables to avoid
acting on the wrong repository. External actions have deadlines and cancellation
cleanup, but an interrupted action may already have taken effect; inspect the
target before retrying. hd/Herdr actions require UTF-8 paths without CR/LF;
Enter's shell protocol preserves native Unix paths, including trailing newlines.

Linux has not been verified. Windows is unsupported, and no Fish wrapper is
provided. A blocked operating-system filesystem call cannot always be cancelled;
SIGKILL cannot guarantee terminal cleanup. Path revalidation is not a sandbox
against a hostile filesystem.

## Development

```sh
cargo build --locked --release
./target/release/qnav --help
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`cargo build` writes to `target/`; `cargo install` installs the executable.
`Cargo.lock` is tracked for reproducible dependency versions. The implementation
uses Rust, Ratatui, Crossterm, and bounded worker threads; there is no async runtime
or database dependency.

Terminal checks require Python 3.10+, Bash, Zsh, and Git. The Python helpers use
only the standard library. They create isolated fixtures and mock hd/Herdr; they
do not control your real workspaces.

```sh
python3 tests/pty_smoke.py --binary target/release/qnav
python3 tests/pty_actions.py --binary target/release/qnav
cargo test --locked --release --test matching_latency -- --ignored --nocapture
python3 tests/pty_benchmark.py --samples 100 --max-warm-p95 50
```

Generated terminal captures and benchmark results go to `test-artifacts/` and are
ignored by Git. Benchmark timings include the PTY driver and terminal parser;
they are not measurements of physical display latency.

## Uninstall

Remove the wrapper's `source` line and any optional qnav alias from your shell
startup file, then run `cargo uninstall qnav`. Start a new shell to drop the
previously loaded function. Keep or remove the qnav configuration separately;
uninstalling does not delete any of your pinned directories.
