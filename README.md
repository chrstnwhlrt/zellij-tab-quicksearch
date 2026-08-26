# zellij-tab-quicksearch

A floating, fuzzy-matching tab picker for [Zellij](https://zellij.dev) — open with one keystroke, type to filter, jump with Enter.

![Rust](https://img.shields.io/badge/rust-1.95+-orange.svg)
![Target](https://img.shields.io/badge/target-wasm32--wasip1-blue.svg)
![Zellij](https://img.shields.io/badge/zellij-0.44+-green.svg)
![License](https://img.shields.io/badge/license-MIT-lightgrey.svg)

## Features

- **Fuzzy search** powered by [nucleo-matcher](https://github.com/helix-editor/nucleo) (Helix's matcher) — smart case, unicode normalization, match highlighting. nucleo's query syntax works as-is: words separated by spaces must all match, `^foo` anchors at the start, `foo$` at the end, `'foo` requires a substring, `!foo` excludes.
- **Typo tolerance** — tabs that nucleo cannot match as a subsequence get a second chance with a bounded edit distance (one typo up to 5 query chars, two up to 9, three beyond), so `bakkend` or `bacckend` still find `backend`. These matches rank below every exact fuzzy match; queries of one or two characters and queries using nucleo's operator syntax stay exact.
- **Frequency-based default order** — most-frequently-jumped tabs rise to the top when the query is empty. In-memory, session-scoped.
- **Instant jump** — pressing a digit on an empty query jumps to that tab by position. With ≥10 tabs the picker buffers digits and jumps once the position is unambiguous: the last possible digit is typed, or 0.4 s pass after the penultimate digit.
- **Active tab as separate header** — you can't jump to the tab you're already on, so it's shown above the list, not inside it.
- **Bell indicator** — tabs with an active bell notification get a blinking light-grey background highlight (match highlights keep the background); the animation pauses automatically when you click into the pane behind the picker, and — on Zellij versions that send `Visible` events to floating panes (after 0.45.0) — as soon as you switch to another tab or detach.
- **Mouse** — the wheel moves the selection, a click selects a row, a second click on the selected row jumps.
- **Pane count per tab** — `[N]` on the right, with the picker-plugin itself subtracted from the active tab's count.
- **Theme-adaptive colours** — picker chrome uses 16-ANSI codes so it follows your Zellij theme; the bell highlight uses 256-colour palette 250 (a soft, near-white background).
- **Wide-character aware** — tab names with emoji or CJK characters are measured by display width ([unicode-width](https://crates.io/crates/unicode-width)), so the frame and truncation stay aligned.
- **Frame-less** — `set_pane_borderless` removes the default Zellij frame for a clean modal look.
- **Configurable size** — `max_cols` and `max_rows` for the floating pane, settable via the plugin alias.
- **Query auto-clear** — fresh query on every open.
- **Graceful on tiny panes** — degrades to blank rather than breaking the frame.

## Keybindings (inside the picker)

| Key                               | Action                                                              |
| --------------------------------- | ------------------------------------------------------------------- |
| Any printable character           | Append to query (fuzzy filter, typo-tolerant from 3 chars)          |
| Digits (empty query)              | Jump to tab N. With ≥10 tabs, digits buffer up to 0.4 s.            |
| `↑` / `Ctrl-p` / `Ctrl-k`         | Move selection up                                                   |
| `↓` / `Tab` / `Ctrl-n` / `Ctrl-j` | Move selection down                                                 |
| `Enter`                           | Switch to the selected tab and close                                |
| `Backspace`                       | Delete the last character                                           |
| `Ctrl-w`                          | Delete the previous word                                            |
| `Ctrl-u`                          | Clear the entire query                                              |
| `Esc` / `Ctrl-c` / `Ctrl-g`       | Close the picker without switching                                  |
| Mouse wheel                       | Move selection one row per wheel step (no wrap-around)              |
| Left click                        | Select the row; click the selected row again to switch (the first row starts selected, so one click there switches) |

## Installation

### Prerequisite

Zellij **0.44+** (for the `set_pane_borderless` plugin API). Built against `zellij-tile` 0.45; every command and event the plugin uses is wire-identical between 0.44.x and 0.45.x, so one build serves both.

### Option A — Nix flake

Build the WASM artefact and symlink it into Zellij's plugin directory:

```bash
nix build
mkdir -p ~/.config/zellij/plugins
ln -sfn "$PWD/result/bin/tab-quicksearch.wasm" \
        ~/.config/zellij/plugins/tab-quicksearch.wasm
```

The resulting Nix-store path is also exposed as `packages.default` and via `overlays.default` for downstream flakes.

### Option B — Cargo / manual build

```bash
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1
cp target/wasm32-wasip1/release/zellij-tab-quicksearch.wasm \
   ~/.config/zellij/plugins/tab-quicksearch.wasm
```

### Option C — Termux (Android)

Termux ships its Rust toolchain via `pkg` instead of `rustup`. The compiler and the WASI target's `std` come from two separate packages that **must be the same version** — otherwise the build fails with a flood of `cannot find Some/Option/Result in this scope` errors (the prelude can't be loaded because the metadata format mismatches).

```bash
pkg install rust rust-std-wasm32-wasip1
cargo build --release --target wasm32-wasip1
install -m 0644 target/wasm32-wasip1/release/zellij-tab-quicksearch.wasm \
        ~/.config/zellij/plugins/tab-quicksearch.wasm
```

If `pkg install rust-std-wasm32-wasip1` pulls a newer version than the already-installed `rust`, run `pkg upgrade rust` (and any other `rust-std-*` packages) first to keep them in lockstep.

## Zellij configuration

Add the plugin alias and a keybind to `~/.config/zellij/config.kdl`. Adjust to taste — Zellij's KDL requires a trailing `;` after each action inside a `bind` block:

```kdl
plugins {
    // existing built-ins …
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

### Reload after rebuilding

Existing Zellij sessions cache the loaded plugin. After rebuilding:

```bash
zellij action start-or-reload-plugin file:~/.config/zellij/plugins/tab-quicksearch.wasm
```

Run it from inside the session (or pass `--session <name>`) while a client is attached: Zellij 0.44/0.45 refuses to reload without one (`No connected clients, cannot reload plugin` in the log) and, worse, has already marked the plugin as pending at that point, so the running instance stops reacting until a later reload succeeds or the session restarts.

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

The plugin requests the floating pane to be sized exactly to `max_cols × max_rows` via Zellij's plugin API; Zellij centres it in the viewport. If the pane is briefly larger during initial layout, the inner box stays capped at those values and is centered inside the pane. Values below 20 columns or 4 rows are clamped; a *pane* smaller than that renders blank rather than breaking the frame.

> **Note**: Zellij re-inserts the floating pane at its default geometry (half the viewport, with a frame) on every open — the plugin only hides itself when closed, and `LaunchOrFocusPlugin` adds it back as if it were new. The plugin re-asserts its size and borderless state as soon as Zellij reports the pane back in the floating layer, so each open may show one frame in Zellij's default size before the target geometry takes effect. On a terminal too small for the configured size the pane is clamped to the viewport and the box shrinks with it; once the terminal grows again the target size is restored.

## Development

### Dev shell

```bash
nix develop
```

Provides the pinned Rust toolchain (`fenix` stable), the `wasm32-wasip1` target, `clippy`, `rustfmt`, `rust-analyzer`, `zellij` (for integration testing), `wabt` (for WASM inspection), `nixpkgs-fmt`, plus `pkg-config` + `openssl` (so the host-side `cargo test` can build `zellij-utils`).

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
