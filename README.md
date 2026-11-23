# zellij-tab-quicksearch

A floating, fuzzy-matching tab picker for [Zellij](https://zellij.dev): open it with one keystroke, type to filter, jump with Enter.

![Rust](https://img.shields.io/badge/rust-1.95+-orange.svg)
![Target](https://img.shields.io/badge/target-wasm32--wasip1-blue.svg)
![Zellij](https://img.shields.io/badge/zellij-0.44+-green.svg)
![License](https://img.shields.io/badge/license-MIT-lightgrey.svg)

## Features

- **Fuzzy search** powered by [nucleo-matcher](https://github.com/helix-editor/nucleo) (Helix's matcher), with smart case, Unicode normalization and match highlighting. nucleo's query syntax works as-is: words separated by spaces must all match, `^foo` anchors at the start, `foo$` at the end, `'foo` requires a substring, `!foo` excludes.
- **Typo tolerance**: tabs that nucleo cannot match as a subsequence get a second chance with a bounded edit distance (one typo up to 5 query chars, two up to 9, three beyond), so `bakkend` or `bacckend` still find `backend`. These matches rank below every regular fuzzy match; queries of one or two characters and queries using nucleo's operator syntax get no typo tolerance.
- **Attention state per tab** (Zellij 0.46+): the marker after the tab number shows the state Zellij tracks for the tab, with the symbols of Zellij's own tab bar: `●` needs you, `✗` failed, `✓` done, `◐` working, a dim `⊘` for a muted tab. Zellij derives it from what programs such as coding agents report about themselves (a program needs you while it waits for approval, an answer or a login), from desktop notifications, fired watches and the terminal bell (all three count as "needs you"), and from the commands it sees running and finishing. Zellij versions before 0.46 send no attention state; there only the bell marks a tab.
- **Blinking only when it counts** (Zellij 0.46+): a tab blinks with a light-grey background, and leads the list on an empty query, while a program in it waits for your approval, an answer or a login, and after it has finished or failed until you have looked at it. Coding agents such as Claude Code report these states to Zellij on their own. A desktop notification or a fired watch makes a tab blink too; the terminal bell does so only when it comes from such a program. Other bells, finished shell commands and working programs just show their marker, and before Zellij 0.46 nothing blinks. The blinking pauses while the pane behind the picker has the focus and, from Zellij 0.45.1, while you are on another tab or detached.
- **Frequency-based default order**: on an empty query, after the blinking tabs, the tabs you jump to most often come first. The counts live in memory for the session.
- **Instant jump**: pressing a digit on an empty query jumps to that tab by position. With 10 or more tabs the picker buffers digits: it jumps once the number has as many digits as the highest tab number, or 0.4 s after the number reached one digit less.
- **Active tab as a separate header**: you can't jump to the tab you're already on, so it's shown above the list (with its marker), not inside it.
- **Mouse**: the wheel moves the selection, a click selects a row, a second click on the selected row jumps.
- **Pane count per tab**: `[N]` on the right, with the picker plugin itself subtracted from the active tab's count.
- **Theme-adaptive colours**: the picker chrome uses the 16 ANSI colours, so it follows your Zellij theme; the blink highlight uses colour 250 of the 256-colour palette (a soft, near-white background). Markers take the row's text colour, bold when urgent (needs you, failed), dim when muted, as in Zellij's tab bar.
- **Wide-character aware**: tab names with emoji or CJK characters are measured by display width ([unicode-width](https://crates.io/crates/unicode-width)), so the frame and truncation stay aligned.
- **Frameless**: `set_pane_borderless` removes the default Zellij frame for a clean modal look.
- **Configurable size**: `max_cols` and `max_rows` for the floating pane, settable via the plugin alias.
- **Esc keeps your place**: closing with `Esc` clears the query, but a selection you moved, or reached by typing, stays on its tab for the next open. After a jump the next open starts at the top of the list.
- **Graceful on tiny panes**: degrades to blank rather than breaking the frame.

## Keybindings (inside the picker)

| Key                                | Action                                                              |
| ---------------------------------- | ------------------------------------------------------------------- |
| Any printable character            | Append to the query (fuzzy filter, typo-tolerant from 3 chars)      |
| Digits (empty query)               | Jump to tab N. With 10 or more tabs, digits buffer (see Instant jump). |
| `Up` / `Ctrl-p` / `Ctrl-k`         | Move selection up                                                   |
| `Down` / `Tab` / `Ctrl-n` / `Ctrl-j` | Move selection down                                               |
| `Enter`                            | Switch to the selected tab and close                                |
| `Backspace`                        | Delete the last character                                           |
| `Ctrl-w`                           | Delete the previous word                                            |
| `Ctrl-u`                           | Clear the entire query                                              |
| `Esc` / `Ctrl-c` / `Ctrl-g`        | Close without switching; the query is cleared, a selection you made stays on its tab |
| Mouse wheel                        | Move selection one row per wheel step (no wrap-around)              |
| Left click                         | Select the row; click the selected row again to switch (the row that starts selected, the first one or the one `Esc` kept, switches with one click) |

## Installation

### Prerequisite

Zellij **0.44+** (for the `set_pane_borderless` plugin API). The plugin is built against `zellij-tile` 0.46, and every command and event it uses is wire-compatible from Zellij 0.44 to 0.46, so one build serves all of them; the status markers and the blinking need a 0.46 host. A build made before 0.46 also keeps running on Zellij 0.46, which detects it and switches to a 0.45 compatibility mode, but it sees no attention state.

### Option A: Nix flake

Build the WASM artefact and symlink it into Zellij's plugin directory:

```bash
nix build
mkdir -p ~/.config/zellij/plugins
ln -sfn "$PWD/result/bin/tab-quicksearch.wasm" \
        ~/.config/zellij/plugins/tab-quicksearch.wasm
```

The package is also exposed as `packages.default` and via `overlays.default` for downstream flakes.

### Option B: Cargo / manual build

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
cp target/wasm32-wasip1/release/zellij-tab-quicksearch.wasm \
   ~/.config/zellij/plugins/tab-quicksearch.wasm
```

### Option C: Termux (Android)

Termux ships its Rust toolchain via `pkg` instead of `rustup`. The compiler and the WASI target's `std` come from two separate packages that **must be the same version**; otherwise the build fails with a flood of `cannot find Some/Option/Result in this scope` errors (the prelude can't be loaded because the metadata formats don't match).

```bash
pkg install rust rust-std-wasm32-wasip1
cargo build --release --target wasm32-wasip1
install -m 0644 target/wasm32-wasip1/release/zellij-tab-quicksearch.wasm \
        ~/.config/zellij/plugins/tab-quicksearch.wasm
```

If `pkg install rust-std-wasm32-wasip1` pulls a newer version than the already-installed `rust`, run `pkg upgrade rust` (and any other `rust-std-*` packages) first to keep them in lockstep.

## Zellij configuration

Add the plugin alias and a keybind to `~/.config/zellij/config.kdl`. Adjust to taste; Zellij's KDL requires a trailing `;` after each action inside a `bind` block:

```kdl
plugins {
    // existing built-ins ...
    tab-quicksearch location="file:~/.config/zellij/plugins/tab-quicksearch.wasm"
}

keybinds {
    shared_except "locked" {
        bind "Alt /" {
            LaunchOrFocusPlugin "tab-quicksearch" {
                floating true
                move_to_focused_tab true
            };
            SwitchToMode "Normal";
        }
    }
}
```

Then open the picker with `Alt /`.

The attention markers and the blinking rely on Zellij's `attention` settings, which are on by default. With `enabled false` or `tab_markers false` Zellij hands plugins no tab status, only whether a tab is muted: apart from the muted mark only the bell marks a tab, and nothing blinks.

### Reload after rebuilding

A running session keeps the plugin it has loaded, and `Esc` only hides the picker. To switch a session to a new build, close every picker instance, then open the picker once while bypassing Zellij's plugin cache. Run this from inside the session; it needs `jq`:

```bash
zellij action list-panes --json |
  jq -r '.[] | select(.plugin_url // "" | contains("tab-quicksearch")) | "zellij action close-pane --pane-id plugin_\(.id)"' |
  sh
zellij action launch-or-focus-plugin --skip-plugin-cache --floating --move-to-focused-tab tab-quicksearch
```

The picker opens with the new build, and from then on the keybinding finds this instance. The frequency counts start over. Without `--skip-plugin-cache` Zellij would start the old build again, because it keeps the compiled plugin in memory after the last instance is closed. The snippet runs unchanged in POSIX shells and in nushell.

Do not use `zellij action start-or-reload-plugin` for this. It does not always find the picker the keybinding opened, and then starts a second, tiled picker next to it.

## Plugin-side configuration

Optional values passed via the plugin alias. Defaults are sensible for typical terminals.

```kdl
plugins {
    tab-quicksearch location="file:~/.config/zellij/plugins/tab-quicksearch.wasm" {
        max_cols "60"
        max_rows "20"
    }
}
```

| Key        | Default | Meaning                          |
| ---------- | ------- | -------------------------------- |
| `max_cols` | 60      | Box width in cells (minimum 20)  |
| `max_rows` | 20      | Box height in cells (minimum 4)  |

The plugin asks Zellij to size the floating pane to exactly `max_cols` x `max_rows`, and Zellij centres it in the viewport. If the pane stays larger (for example made fullscreen), the inner box stays capped at those values and is centred inside the pane. Values below 20 columns or 4 rows are clamped; a *pane* smaller than that renders blank rather than breaking the frame.

> **Note**: Zellij re-inserts the floating pane at its default geometry (half the viewport) on every open, because the plugin only hides itself when closed and `LaunchOrFocusPlugin` adds it back as if it were new. As soon as Zellij renders the plugin at that larger size, the plugin requests its own size, which Zellij usually applies before the next screen update. Until the pane has shrunk, the plugin draws the box in the pane's top-left corner at exactly the box size: Zellij keeps a pane's content when it shrinks the pane and re-wraps every line that no longer fits, so a centred box would show up scrambled until the next render. When Zellij then reports the pane back in the floating layer, the plugin re-asserts size and borderless state once more. On a terminal too small for the configured size the pane is clamped to the viewport and the box shrinks with it; once the terminal grows again the target size is restored. The plugin re-sends the size at most three times until the pane reaches it, since Zellij 0.46 answers every resize with a fresh pane report.

## Development

### Dev shell

```bash
nix develop
```

Provides the pinned Rust toolchain (`fenix` stable), the `wasm32-wasip1` target, `clippy`, `rustfmt`, `rust-analyzer`, `zellij` (for integration testing), `wabt` (for WASM inspection), `nixpkgs-fmt`, plus `pkg-config` + `openssl` (so the host-side `cargo test` can build `zellij-utils`; its bundled SQLite is compiled with the shell's C compiler).

### Common commands

```bash
cargo build --release --target wasm32-wasip1   # build WASM
cargo clippy --target wasm32-wasip1 --release -- -W clippy::pedantic -D warnings
nix develop -c cargo clippy --all-targets -- -W clippy::pedantic -D warnings   # host build incl. tests
nix develop -c cargo test                      # unit tests (host build; needs the devShell's openssl/pkg-config)
cargo fmt
nix build           # reproducible release build
nix flake check     # evaluate + build as CI gate
nix fmt             # format flake.nix
```

### Project layout

```
zellij-tab-quicksearch/
├── Cargo.toml        # crate metadata, 3 runtime dependencies
├── Cargo.lock        # checked in for reproducible builds
├── flake.nix         # Nix package + devShell + overlay (fenix toolchain)
├── flake.lock
├── .gitignore
├── LICENSE
├── README.md
└── src/
    └── main.rs       # single-file plugin + unit tests
```

## License

[MIT](./LICENSE) © Christian Wohlert
