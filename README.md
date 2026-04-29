# zellij-tab-quicksearch

A floating, fuzzy-matching tab picker for [Zellij](https://zellij.dev) — open with one keystroke, type to filter, jump with Enter.

![Rust](https://img.shields.io/badge/rust-1.95+-orange.svg)
![Target](https://img.shields.io/badge/target-wasm32--wasip1-blue.svg)
![Zellij](https://img.shields.io/badge/zellij-0.44+-green.svg)
![License](https://img.shields.io/badge/license-MIT-lightgrey.svg)

## Features

- **Fuzzy search** powered by [nucleo-matcher](https://github.com/helix-editor/nucleo) (Helix's matcher) — smart case, unicode normalization, match highlighting.
- **Frequency-based default order** — most-frequently-jumped tabs rise to the top when the query is empty. In-memory, session-scoped.
- **Instant jump** — pressing a digit on an empty query jumps to that tab by position. With 10+ tabs the picker buffers digits until the full position is typed or 0.4 s pass since the last keystroke.
- **Active tab as separate header** — you can't jump to the tab you're already on, so it's shown above the list, not inside it.
- **Bell indicator** — tabs with an active bell notification get a blinking light-grey background highlight; the animation pauses automatically when you click into the pane behind the picker.
- **Pane count per tab** — `[N]` on the right, with the picker-plugin itself subtracted from the active tab's count.
- **Theme-adaptive colours** — picker chrome uses 16-ANSI codes so it follows your Zellij theme; the bell highlight uses 256-colour palette 250 (a soft, near-white background).
- **Frame-less** — `set_pane_borderless` removes the default Zellij frame for a clean modal look.
- **Configurable size** — `max_cols` and `max_rows` for the floating pane, settable via the plugin alias.
- **Query auto-clear** — fresh query on every open.
- **Graceful on tiny panes** — degrades to blank rather than breaking the frame.

## Keybindings (inside the picker)

| Key                               | Action                                                              |
| --------------------------------- | ------------------------------------------------------------------- |
| Any printable character           | Append to query (fuzzy filter)                                      |
| Digits (empty query)              | Jump to tab N. With ≥10 tabs, digits buffer up to 0.4 s.            |
| `↑` / `Ctrl-p` / `Ctrl-k`         | Move selection up                                                   |
| `↓` / `Tab` / `Ctrl-n` / `Ctrl-j` | Move selection down                                                 |
| `Enter`                           | Switch to the selected tab and close                                |
| `Backspace`                       | Delete the last character                                           |
| `Ctrl-w`                          | Delete the previous word                                            |
| `Ctrl-u`                          | Clear the entire query                                              |
| `Esc` / `Ctrl-c` / `Ctrl-g`       | Close the picker without switching                                  |

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
| `max_cols` | 60      | Box width in cells               |
| `max_rows` | 20      | Box height in cells              |

The plugin requests the floating pane to be sized exactly to `max_cols × max_rows` via Zellij's plugin API. If the pane is briefly larger during initial layout, the inner box stays capped at those values and is centered inside the pane. Below 20 columns or 4 rows the picker renders blank rather than breaking the frame.

> **Note**: Zellij 0.44 has a race condition between plugin loading and floating-pane resize requests, which causes a brief one-frame flicker on the first open: the pane is initially shown in Zellij's default size before the plugin's resize takes effect. Subsequent opens are stable.

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
    └── main.rs       # single-file plugin (~1130 lines)
```

## License

[MIT](./LICENSE) © Christian Wohlert
