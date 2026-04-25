# zellij-tab-quicksearch

A floating, fuzzy-matching tab picker for [Zellij](https://zellij.dev) — open with one keystroke, type to filter, jump with Enter.

![Rust](https://img.shields.io/badge/rust-1.95+-orange.svg)
![Target](https://img.shields.io/badge/target-wasm32--wasip1-blue.svg)
![Zellij](https://img.shields.io/badge/zellij-0.44+-green.svg)
![License](https://img.shields.io/badge/license-MIT-lightgrey.svg)

## Features

- **Fuzzy search** powered by [nucleo-matcher](https://github.com/helix-editor/nucleo) (Helix's matcher) — smart case, unicode normalization, match highlighting.
- **Frequency-based default order** — most-frequently-jumped tabs rise to the top when the query is empty. In-memory, session-scoped.
- **Instant jump** — with an empty query, `1`–`9` jumps directly to that tab position.
- **Active tab as separate header** — you can't jump to the tab you're already on, so it's shown above the list, not inside it.
- **Bell indicator** — tabs with an active bell notification get a yellow `!` marker.
- **Pane count per tab** — `[N]` on the right, with the picker-plugin itself subtracted from the active tab's count.
- **Theme-adaptive colours** — uses 16-ANSI colour codes so the picker follows your Zellij theme.
- **Frame-less** — `set_pane_borderless` removes the default Zellij frame for a clean modal look.
- **Configurable size** — min/max absolute + max percentage for columns and rows, settable via the plugin alias.
- **Query auto-clear** — fresh query on every open.
- **Graceful on tiny panes** — degrades to blank rather than breaking the frame.

## Keybindings (inside the picker)

| Key                               | Action                                                     |
| --------------------------------- | ---------------------------------------------------------- |
| Any printable character           | Append to query (fuzzy filter)                             |
| `1`–`9` (only with empty query)   | Instant jump to that tab position                          |
| `↑` / `Ctrl-p` / `Ctrl-k`         | Move selection up                                          |
| `↓` / `Tab` / `Ctrl-n` / `Ctrl-j` | Move selection down                                        |
| `Enter`                           | Switch to the selected tab and close                       |
| `Backspace`                       | Delete the last character                                  |
| `Ctrl-w`                          | Delete the previous word                                   |
| `Ctrl-u`                          | Clear the entire query                                     |
| `Esc` / `Ctrl-c` / `Ctrl-g`       | Close the picker without switching                         |

## Installation

### Prerequisite

Zellij **0.44+** (for the `set_pane_borderless` plugin API).

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

## Plugin-side configuration

Optional values passed via the plugin alias. Defaults are sensible for typical terminals.

```kdl
plugins {
    tab-quicksearch location="file:~/.config/zellij/plugins/tab-quicksearch.wasm" {
        min_cols "40"
        max_cols "90"
        max_cols_percent "60"

        min_rows "10"
        max_rows "22"
        max_rows_percent "50"
    }
}
```

| Key                | Default | Meaning                                                       |
| ------------------ | ------- | ------------------------------------------------------------- |
| `min_cols`         | 40      | Minimum width of the inner box                                |
| `max_cols`         | 90      | Hard cap on width                                             |
| `max_cols_percent` | 60      | Width as percentage of the pane, upper-bounded by `max_cols`  |
| `min_rows`         | 10      | Minimum height                                                |
| `max_rows`         | 22      | Hard cap on height                                            |
| `max_rows_percent` | 50      | Height as percentage of the pane                              |

Effective size per dimension is `clamp(avail, min_abs, min(max_abs, avail * max_percent / 100))`. Below 20 columns the picker renders blank rather than breaking the frame.

## Development

### Dev shell

```bash
nix develop
```

Provides the pinned Rust toolchain (`fenix` stable), the `wasm32-wasip1` target, `clippy`, `rustfmt`, `rust-analyzer`, `zellij` (for integration testing), `wabt` (for WASM inspection) and `nixpkgs-fmt`.

### Common commands

```bash
cargo build --release --target wasm32-wasip1   # build WASM
cargo clippy --target wasm32-wasip1 --release -- -W clippy::pedantic -D warnings
cargo fmt
nix build           # reproducible release build
nix flake check     # evaluate + build as CI gate
nix fmt             # format flake.nix
```

### Project layout

```
zellij-tab-quicksearch/
├── Cargo.toml        # crate metadata, 2 runtime dependencies
├── Cargo.lock        # checked in for reproducible builds
├── flake.nix         # Nix package + devShell + overlay (fenix toolchain)
├── flake.lock
├── .gitignore
├── LICENSE
├── README.md
└── src/
    └── main.rs       # single-file plugin (~820 lines)
```

## License

[MIT](./LICENSE) © Christian Wohlert
