use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::rc::Rc;
use zellij_tile::prelude::*;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};
use unicode_width::UnicodeWidthChar;

// SGR sequences. The picker chrome uses 16-ANSI palette codes so it
// follows the active Zellij theme; only `BELL` reaches into 256-colour
// for a soft, theme-neutral light-grey background.
const RESET: &str = "\u{1b}[0m";
const DIM: &str = "\u{1b}[2m";
const REVERSE: &str = "\u{1b}[7m";
const PROMPT: &str = "\u{1b}[1;36m"; // bold cyan
const MATCH: &str = "\u{1b}[1;33m"; // bold yellow
const FRAME: &str = "\u{1b}[2m";
const ACTIVE: &str = "\u{1b}[1;36m"; // bold cyan
const SELECTED: &str = "\u{1b}[1;7m"; // bold + reverse — selection highlight

// Bell: reverse flag + light-grey fg → the swap makes fg = default-bg
// (same fg as selection) and bg = light grey (palette 250, near white).
const BELL: &str = "\u{1b}[7;38;5;250m";

// Static padding to avoid `" ".repeat(n)` allocations in hot paths.
const PAD_SPACES: &str = "                                                                                                                                ";

/// Slice for small known n (e.g. format! inserts). For arbitrary n
/// (terminal widths > 128) use `push_pad` — it loops correctly.
fn pad(n: usize) -> &'static str {
    &PAD_SPACES[..n.min(PAD_SPACES.len())]
}

/// Writes exactly `n` spaces into `out`, even when `n > PAD_SPACES.len()`.
/// Important for ultrawide terminals so lead/trail/blank rows are long
/// enough — otherwise they would render as transparent cells.
fn push_pad(out: &mut String, n: usize) {
    let mut remaining = n;
    while remaining >= PAD_SPACES.len() {
        out.push_str(PAD_SPACES);
        remaining -= PAD_SPACES.len();
    }
    if remaining > 0 {
        out.push_str(&PAD_SPACES[..remaining]);
    }
}

// Hard upper bound for the query — prevents layout overflow on paste of
// huge strings and preserves prompt-row integrity.
const MAX_QUERY_CHARS: usize = 128;

// Blink interval for the bell background (toggle per tick).
const BLINK_INTERVAL: f64 = 0.5;

// Time after the (D-1)-th digit before a quickjump fires on the buffered
// number. Picked to be long enough for the user to type the final digit
// of a known multi-digit position, but short enough to feel responsive
// for a deliberate single-digit jump.
const JUMP_TIMEOUT: f64 = 0.4;

#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

struct TabEntry {
    info: Rc<TabInfo>,
    haystack: Utf32String,
}

struct Scored {
    score: u32,
    tab: Rc<TabInfo>,
    indices: Vec<u32>,
}

/// Configured box dimensions in cells. Sent both as the pane request size
/// to `change_floating_panes_coordinates` and used as the render-box cap:
/// the plugin box is `min(render_area, size)`, centered inside the pane.
struct SizeCfg {
    cols: usize,
    rows: usize,
}

impl Default for SizeCfg {
    fn default() -> Self {
        Self { cols: 60, rows: 20 }
    }
}

impl SizeCfg {
    fn from_config(cfg: &BTreeMap<String, String>) -> Self {
        fn parse(cfg: &BTreeMap<String, String>, key: &str, fallback: usize) -> usize {
            cfg.get(key)
                .and_then(|s| s.parse().ok())
                .unwrap_or(fallback)
        }
        let d = Self::default();
        Self {
            cols: parse(cfg, "max_cols", d.cols),
            rows: parse(cfg, "max_rows", d.rows),
        }
    }
}

/// Plugin visibility, derived from two independent indicators that must
/// both be true (`visible = pane_focused && float_visible`):
///
/// - `pane_focused` (`PaneUpdate`): plugin pane is in the floating layer
///   and focused.
/// - `float_visible` (`TabUpdate.are_floating_panes_visible`): the
///   floating layer is shown in the user's active tab. Flips to `false`
///   when the user clicks into the tile below without changing
///   `pane_focused` — so `pane_focused` alone is not sufficient.
///
/// Known limitation: when the user switches away from the plugin's tab,
/// the plugin's view freezes. Zellij filters `PaneUpdate`/`TabUpdate` to
/// plugins in the client's active tab (`screen.rs:3310`) and does not
/// send `Event::Visible(false)` to floating panes (`tab/mod.rs:4707`).
/// The blink loop then keeps running invisibly (not perceivable by the
/// user, CPU-irrelevant) and terminates on return via `TabUpdate`.
#[derive(Default)]
struct Visibility {
    visible: bool,
    pane_focused: bool,
    float_visible: bool,
}

/// `Timer`-related flags. zellij-tile exposes no cancel API, so these
/// flags are the only mechanism to gate live vs. stale `Timer` events.
#[derive(Default)]
struct Timers {
    /// Toggles per blink tick — drives the bell-row background colour.
    bell_blink_on: bool,
    /// Singleton flag: exactly one blink `Timer` is in flight when true.
    /// See `State::hide` for why we never reset this on close.
    blink_scheduled: bool,
    /// True while a quickjump final-digit `Timer` is in flight.
    jump_pending: bool,
}

#[derive(Default)]
struct State {
    tabs: Vec<TabEntry>,
    query: String,
    selected: usize,
    scroll_offset: usize,
    matcher: Matcher,
    scored: Vec<Scored>,
    size: SizeCfg,
    /// True after `PermissionStatus::Granted` — gates Key/PaneUpdate handling.
    ready: bool,
    /// Plugin's own pane id, captured in `Granted` from `get_plugin_ids()`.
    /// Needed for `change_floating_panes_coordinates` and `set_pane_borderless`.
    plugin_id: u32,
    timers: Timers,
    vis: Visibility,
    /// Access frequency via the picker (Enter / instant-jump). Most-used
    /// tab first. The active tab is filtered out before sorting and shown
    /// as a header. Pruned to live tabs on each `TabUpdate`.
    access_counts: BTreeMap<usize, u64>,
    /// Last floating-pane size observed via `PaneUpdate`. Debounces resize
    /// requests: we only re-issue `change_floating_panes_coordinates` when the
    /// size is off-target AND changed since last seen. On a terminal smaller
    /// than the box Zellij clamps the pane to the viewport, so it can never
    /// reach the target — without this guard every `PaneUpdate` would fire
    /// another doomed resize. A real layout reset still changes the size.
    last_pane_size: Option<(usize, usize)>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::TabUpdate,
            // `PaneUpdate`: re-resize on Zellij's layout resets, plus the
            // `pane_focused` indicator (one of two visibility components —
            // the other is `are_floating_panes_visible` from `TabUpdate`).
            EventType::PaneUpdate,
            EventType::Key,
            // `Timer`: quickjump buffer + bell-blink loop.
            EventType::Timer,
        ]);
        self.size = SizeCfg::from_config(&configuration);
        // No `hide_self()` here: it would race with Zellij's
        // `LaunchOrFocusPlugin` show on the first open and the plugin
        // would vanish immediately. `render()` already returns blank
        // while `!ready`.
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                let ids = get_plugin_ids();
                self.plugin_id = ids.plugin_id;
                self.ready = true;
                // Resize + borderless straight from `Granted` so the pane
                // reaches its target geometry one round-trip earlier than
                // waiting for the first `PaneUpdate`. The `PaneUpdate`
                // handler still re-issues the resize on later layout
                // resets, so this is a one-time fast-path, not a
                // replacement.
                self.resize_pane();
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                // Without ChangeApplicationState we can neither set
                // borderless nor switch tabs. Close the plugin instead of
                // hanging silently.
                close_self();
                false
            }
            Event::TabUpdate(new_tabs) => {
                // Dedup: Zellij fires `TabUpdate` often without changes.
                // `PartialEq` on `TabInfo` covers all fields, including
                // `are_floating_panes_visible` and `has_bell_notification`,
                // so it is safe to bail out early here.
                if self.tabs.len() == new_tabs.len()
                    && new_tabs.iter().zip(&self.tabs).all(|(n, e)| *n == *e.info)
                {
                    return false;
                }
                // `are_floating_panes_visible` from the active tab
                // co-determines plugin visibility (a click into the
                // background pane disables the floating layer without
                // changing `is_focused`).
                self.vis.float_visible = new_tabs
                    .iter()
                    .find(|t| t.active)
                    .is_some_and(|t| t.are_floating_panes_visible);
                // Cache `Utf32String` fuzzy haystacks once per `TabUpdate`
                // (previously: per keystroke × tab).
                self.tabs = new_tabs
                    .into_iter()
                    .map(|t| TabEntry {
                        haystack: Utf32String::from(t.name.as_str()),
                        info: Rc::new(t),
                    })
                    .collect();
                // Prune frequency counts for tabs that no longer exist so
                // `access_counts` cannot grow unbounded over a long session of
                // tab churn (it is keyed by `tab_id`, which dies with the tab).
                let live: Vec<usize> = self.tabs.iter().map(|e| e.info.tab_id).collect();
                self.access_counts.retain(|id, _| live.contains(id));
                self.refresh_scores();
                self.recompute_visibility();
                // A newly arriving bell may need to start the loop even
                // without a visibility transition — `start_blink_if_needed`
                // is idempotent.
                self.start_blink_if_needed();
                true
            }
            Event::PaneUpdate(manifest) => {
                // `PaneUpdate` provides `pane_focused` (one of the two
                // visibility components — the other is
                // `are_floating_panes_visible` from `TabUpdate`) and
                // re-issues the resize when Zellij has reset the pane
                // geometry (e.g. on layout swap). The initial resize is
                // already done in `Granted` for a fast first paint.
                if !self.ready {
                    return false;
                }
                let pid = self.plugin_id;
                let mut pane_focused_now = false;
                let mut cur_size: Option<(usize, usize)> = None;
                'outer: for pane_infos in manifest.panes.values() {
                    for p in pane_infos {
                        if p.is_plugin && p.id == pid {
                            pane_focused_now = p.is_floating && !p.is_suppressed && p.is_focused;
                            if p.is_floating {
                                cur_size = Some((p.pane_columns, p.pane_rows));
                            }
                            break 'outer;
                        }
                    }
                }
                // Debounce: only re-issue the resize when the floating pane is
                // off-target AND its size changed since the last `PaneUpdate`.
                // See `last_pane_size` for why (terminals smaller than the box).
                if let Some(size) = cur_size {
                    if size != (self.size.cols, self.size.rows) && self.last_pane_size != Some(size)
                    {
                        self.resize_pane();
                    }
                }
                self.last_pane_size = cur_size;
                self.vis.pane_focused = pane_focused_now;
                self.recompute_visibility();
                false
            }
            Event::Key(key) => {
                // Drop keys before permission/show — otherwise the query
                // fills up while the pane is still hidden/initializing
                // and would appear unexpectedly populated on the first
                // render.
                if !self.ready {
                    return false;
                }
                self.handle_key(&key)
            }
            Event::Timer(_) => {
                // Two timer sources share this handler: quickjump and
                // bell-blink. We disambiguate via the state flags.
                let mut rerender = false;

                // Quickjump timer: fires after the (D-1)-th digit. If the
                // query is still a valid jump buffer, jump. The
                // `self.vis.visible` guard rejects stale timers that arrive
                // while the picker is closed (zellij-tile has no cancel
                // API, so closed-state `Timer` events still get delivered).
                if self.timers.jump_pending {
                    self.timers.jump_pending = false;
                    if self.ready && self.vis.visible && is_pending_jump_buffer(&self.query) {
                        if let Ok(pos) = self.query.parse::<u32>() {
                            if self.try_instant_jump(pos) {
                                // Plugin was hidden via `hide_self()` —
                                // no further re-render needed.
                                return false;
                            }
                        }
                    }
                }

                // Bell-blink timer: toggle the bg colour and schedule the
                // next tick when the plugin is visible AND at least one
                // bell tab exists.
                if self.timers.blink_scheduled {
                    self.timers.blink_scheduled = false;
                    if self.vis.visible && self.has_bell_tab() {
                        self.timers.bell_blink_on = !self.timers.bell_blink_on;
                        self.timers.blink_scheduled = true;
                        set_timeout(BLINK_INTERVAL);
                        rerender = true;
                    } else {
                        // Conditions gone — loop terminates. Restarted by
                        // `recompute_visibility` (`became_visible`) or
                        // `TabUpdate` as soon as conditions hold again.
                        self.timers.bell_blink_on = false;
                    }
                }

                rerender
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        // One string buffer, one print! → one syscall instead of ~30.
        let mut out = String::with_capacity(rows * (cols + 8));

        if !self.ready {
            for _ in 0..rows {
                append_padded(&mut out, cols, "");
            }
            if out.ends_with('\n') {
                out.pop();
            }
            print!("{out}");
            return;
        }

        // Box capped at the configured size, centered in the plugin area.
        // When `change_floating_panes_coordinates` has taken effect the
        // render area equals the box → no padding. Otherwise (initial
        // 1-frame race) the box is rendered compact with padding around
        // it — cosmetically imperfect but stable across re-opens.
        let tcols = cols.min(self.size.cols);
        let trows = rows.min(self.size.rows);
        let hpad = cols.saturating_sub(tcols) / 2;
        let vpad_top = rows.saturating_sub(trows) / 2;
        let vpad_bot = rows.saturating_sub(trows).saturating_sub(vpad_top);

        for _ in 0..vpad_top {
            append_padded(&mut out, cols, "");
        }
        self.render_box(&mut out, trows, tcols, hpad, cols);
        for _ in 0..vpad_bot {
            append_padded(&mut out, cols, "");
        }

        // Drop the trailing `\n`: otherwise the plugin writes rows × `\n`,
        // which moves the cursor to row[rows] (outside the plugin area)
        // after the last line and causes vte to scroll the grid up by one
        // row — the top line (`╭───╮`) disappears.
        if out.ends_with('\n') {
            out.pop();
        }

        print!("{out}");
    }
}

fn append_padded(out: &mut String, cols: usize, content: &str) {
    let vis = visible_len(content);
    let n = cols.saturating_sub(vis);
    out.push_str(content);
    push_pad(out, n);
    out.push('\n');
}

/// Per-row highlight context. Bundled so rendering helpers stay under
/// the argument-count limit and to prevent flag-swap bugs at call sites.
#[derive(Clone, Copy, Default)]
struct HighlightStyle {
    /// This row is the picker's selection.
    selected: bool,
    /// This row's tab has a pending bell notification.
    bell: bool,
    /// Current blink tick state (only relevant when `bell`).
    blink_on: bool,
}

/// Writes a framed line with conditional outer highlight.
/// Logic:
/// - Bell off (`bell && !blink_on`): no highlight (selection blinks out,
///   bell bg off). Plain text.
/// - Otherwise: selected dominates (`REVERSE`), else bell bg, else none.
fn append_wrap(out: &mut String, content: &str, content_width: usize, style: HighlightStyle) {
    let vis = visible_len(content);
    let gap = content_width.saturating_sub(vis);
    let lit = if style.bell { style.blink_on } else { true };
    let outer: &str = if lit {
        if style.selected {
            SELECTED
        } else if style.bell {
            BELL
        } else {
            ""
        }
    } else {
        ""
    };

    out.push_str(FRAME);
    out.push('│');
    out.push_str(RESET);
    out.push(' ');
    if !outer.is_empty() {
        out.push_str(outer);
    }
    out.push(' ');
    out.push_str(content);
    out.push_str(pad(gap));
    out.push(' ');
    if !outer.is_empty() {
        out.push_str(RESET);
    }
    out.push(' ');
    out.push_str(FRAME);
    out.push('│');
    out.push_str(RESET);
}

fn append_horizontal_border(out: &mut String, left: char, right: char, cols: usize) {
    out.push_str(FRAME);
    out.push(left);
    for _ in 0..cols.saturating_sub(2) {
        out.push('─');
    }
    out.push(right);
    out.push_str(RESET);
}

impl State {
    /// Writes a horizontal border line (lead + ╭─╮ / ├─┤ / ╰─╯ + trail + \n).
    fn write_border(
        out: &mut String,
        hpad: usize,
        trail_n: usize,
        left: char,
        right: char,
        cols: usize,
    ) {
        push_pad(out, hpad);
        append_horizontal_border(out, left, right, cols);
        push_pad(out, trail_n);
        out.push('\n');
    }

    /// Writes a framed content line (lead + │ + content + │ + trail + \n).
    fn write_wrapped(
        out: &mut String,
        hpad: usize,
        trail_n: usize,
        content: &str,
        content_width: usize,
        style: HighlightStyle,
    ) {
        push_pad(out, hpad);
        append_wrap(out, content, content_width, style);
        push_pad(out, trail_n);
        out.push('\n');
    }

    fn build_prompt(&self, content: &mut String, content_width: usize) {
        // Layout: "❯ " + query + cursor block. Padding to the box edge is
        // handled by append_wrap. visible_len would mis-count wide chars
        // (emojis etc.), so we stay with pure 1-cell content.
        let query_budget = content_width.saturating_sub(3);
        let query_display = query_tail(&self.query, query_budget);

        content.clear();
        content.push_str(PROMPT);
        content.push('❯');
        content.push_str(RESET);
        content.push(' ');
        content.push_str(query_display);
        content.push_str(REVERSE);
        content.push(' ');
        content.push_str(RESET);
    }

    fn render_list(
        &mut self,
        out: &mut String,
        content: &mut String,
        list_rows: usize,
        hpad: usize,
        trail_n: usize,
        content_width: usize,
    ) {
        if self.scored.is_empty() {
            let msg = if self.tabs.is_empty() {
                "no tabs"
            } else {
                "no matches"
            };
            let lpad = content_width.saturating_sub(msg.len()) / 2;
            let mid = list_rows / 2;
            for row in 0..list_rows {
                content.clear();
                if row == mid {
                    push_pad(content, lpad);
                    content.push_str(DIM);
                    content.push_str(msg);
                    content.push_str(RESET);
                }
                Self::write_wrapped(
                    out,
                    hpad,
                    trail_n,
                    content,
                    content_width,
                    HighlightStyle::default(),
                );
            }
            return;
        }

        if self.selected >= self.scroll_offset + list_rows {
            self.scroll_offset = self.selected + 1 - list_rows;
        } else if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }

        let pos_width = digit_count(self.tabs.len());
        let blink_on = self.timers.bell_blink_on;
        for row in 0..list_rows {
            let idx = self.scroll_offset + row;
            content.clear();
            // Single lookup, then derive both row content and outer style.
            // Indices past `scored.len()` simply render as empty rows.
            let style = if let Some(s) = self.scored.get(idx) {
                let is_sel = idx == self.selected;
                render_row(content, s, is_sel, content_width, pos_width, blink_on);
                HighlightStyle {
                    selected: is_sel,
                    bell: s.tab.has_bell_notification,
                    blink_on,
                }
            } else {
                HighlightStyle::default()
            };
            Self::write_wrapped(out, hpad, trail_n, content, content_width, style);
        }
    }

    fn render_box(
        &mut self,
        out: &mut String,
        rows: usize,
        cols: usize,
        hpad: usize,
        outer_cols: usize,
    ) {
        // Minimum size: frame(6) + minimum content. Below 20 cols render
        // blank.
        if rows < 4 || cols < 20 {
            for _ in 0..rows {
                append_padded(out, outer_cols, "");
            }
            return;
        }
        let content_width = cols - 6;
        let active_tab = self
            .tabs
            .iter()
            .find(|e| e.info.active)
            .map(|e| Rc::clone(&e.info));
        let trail_n = outer_cols.saturating_sub(hpad + cols);
        let mut content = String::with_capacity(cols * 2);

        Self::write_border(out, hpad, trail_n, '╭', '╮', cols);

        self.build_prompt(&mut content, content_width);
        Self::write_wrapped(
            out,
            hpad,
            trail_n,
            &content,
            content_width,
            HighlightStyle::default(),
        );

        Self::write_border(out, hpad, trail_n, '├', '┤', cols);

        let mut list_rows = rows.saturating_sub(4);
        if let Some(t) = &active_tab {
            if list_rows >= 2 {
                content.clear();
                render_active_row(&mut content, t, content_width);
                Self::write_wrapped(
                    out,
                    hpad,
                    trail_n,
                    &content,
                    content_width,
                    HighlightStyle::default(),
                );
                Self::write_border(out, hpad, trail_n, '├', '┤', cols);
                list_rows -= 2;
            }
        }

        self.render_list(out, &mut content, list_rows, hpad, trail_n, content_width);

        Self::write_border(out, hpad, trail_n, '╰', '╯', cols);
    }

    /// True when at least one tab has a bell notification.
    fn has_bell_tab(&self) -> bool {
        self.tabs.iter().any(|e| e.info.has_bell_notification)
    }

    /// Recomputes `visible` from the two independent indicators and
    /// starts the blink loop on a `false → true` transition. The
    /// `true → false` transition needs no action of its own: each timer
    /// tick checks at schedule time whether it should still run.
    fn recompute_visibility(&mut self) {
        let new_visible = self.vis.pane_focused && self.vis.float_visible;
        let became_visible = !self.vis.visible && new_visible;
        self.vis.visible = new_visible;
        if became_visible {
            self.start_blink_if_needed();
        }
    }

    /// Starts the bell-blink tick when (a) the plugin is visible, (b) a
    /// bell tab exists, and (c) no tick is already pending.
    fn start_blink_if_needed(&mut self) {
        if !self.vis.visible || self.timers.blink_scheduled || !self.has_bell_tab() {
            return;
        }
        self.timers.blink_scheduled = true;
        set_timeout(BLINK_INTERVAL);
    }

    /// Sends pane geometry + borderless to Zellij. Both calls together,
    /// because `change_pane_coordinates` internally calls
    /// `set_pane_frames`, which resets `content_offset` based on the
    /// `borderless` flag.
    fn resize_pane(&self) {
        if self.size.cols == 0 || self.size.rows == 0 {
            return;
        }
        change_floating_panes_coordinates(vec![(
            PaneId::Plugin(self.plugin_id),
            FloatingPaneCoordinates::default()
                .with_width_fixed(self.size.cols)
                .with_height_fixed(self.size.rows),
        )]);
        set_pane_borderless(PaneId::Plugin(self.plugin_id), true);
    }

    fn hide(&mut self) {
        // Clear the query for the next open cycle; plugin instance and
        // counts persist.
        self.query.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.refresh_scores();
        // Mark invisible locally right away — the blink loop terminates
        // on the next tick. `PaneUpdate`/`TabUpdate` would do the same
        // but arrive delayed. Reset components and result together so
        // the next open starts from a clean slate.
        self.vis.pane_focused = false;
        self.vis.float_visible = false;
        self.vis.visible = false;
        // Reset timer flags so a stale `Timer` event from before close
        // (zellij-tile has no cancel API) finds clean state and no-ops
        // instead of mis-firing into a freshly reopened session.
        //
        // Intentionally NOT reset: `blink_scheduled`. It is the singleton
        // flag that says "exactly one blink `Timer` is in flight".
        // Resetting it on close would let `start_blink_if_needed`
        // schedule a second `Timer` on reopen before the first one fires
        // — both would then perpetually re-schedule each other and the
        // bell would blink at double frequency. The in-flight `Timer`
        // self-terminates safely (visible=false → terminate path), so
        // leaving the flag true is both safe and necessary.
        self.timers.jump_pending = false;
        self.timers.bell_blink_on = false;
        hide_self();
    }

    fn record_access(&mut self, tab_id: usize) {
        *self.access_counts.entry(tab_id).or_insert(0) += 1;
    }

    fn query_changed(&mut self) {
        self.selected = 0;
        self.scroll_offset = 0;
        self.refresh_scores();
    }

    fn delete_word_backward(&mut self) {
        while matches!(self.query.chars().last(), Some(c) if c.is_whitespace()) {
            self.query.pop();
        }
        while matches!(self.query.chars().last(), Some(c) if !c.is_whitespace()) {
            self.query.pop();
        }
        self.query_changed();
    }

    /// Checks whether `c` is the next digit of an in-progress quickjump
    /// buffer: query must be empty (first digit) or already all-digit
    /// non-zero-start, and must not yet have reached the full digit
    /// count `digit_count(tabs.len())`. `c == '0'` as the first digit
    /// falls through to the regular query.
    fn is_jump_pending_for(&self, c: char) -> bool {
        if self.query.is_empty() {
            return c != '0';
        }
        is_pending_jump_buffer(&self.query)
            && self.query.chars().count() < digit_count(self.tabs.len())
    }

    /// Handles a digit input in quickjump mode.
    /// Generic over arbitrary tab counts: the buffer grows eagerly up to
    /// (D-1) digits, then a timer waits for the final digit. On the D-th
    /// digit or on timeout the picker jumps.
    fn handle_jump_digit(&mut self, c: char) {
        self.query.push(c);
        let cur = self.query.chars().count();
        let max = digit_count(self.tabs.len());
        if cur >= max {
            // Final digit reached — position is unambiguous, jump
            // immediately. On success `try_instant_jump` calls `hide()`
            // which clears state, so we return without falling through
            // to `query_changed()`.
            if let Ok(pos) = self.query.parse::<u32>() {
                if self.try_instant_jump(pos) {
                    return;
                }
            }
            // Parse failed or position out of range (e.g. 99 with 50
            // tabs) → treat the buffer as a regular search query.
        } else if cur + 1 == max {
            // Exactly (D-1) digits buffered — start the timer for the
            // final digit.
            self.timers.jump_pending = true;
            set_timeout(JUMP_TIMEOUT);
        }
        self.query_changed();
    }

    /// Jumps to tab position `pos` (1-based). Returns true when the
    /// position exists and the picker has been closed.
    fn try_instant_jump(&mut self, pos: u32) -> bool {
        let Some(e) = self
            .tabs
            .iter()
            .find(|e| e.info.position.checked_add(1) == Some(pos as usize))
        else {
            return false;
        };
        if !e.info.active {
            let tab_id = e.info.tab_id;
            switch_tab_to(pos);
            self.record_access(tab_id);
        }
        self.hide();
        true
    }

    fn handle_key(&mut self, key: &KeyWithModifier) -> bool {
        let has_ctrl = key.key_modifiers.contains(&KeyModifier::Ctrl);
        let has_alt = key.key_modifiers.contains(&KeyModifier::Alt);

        match key.bare_key {
            BareKey::Esc => {
                self.hide();
                false
            }
            BareKey::Char('c' | 'g') if has_ctrl => {
                self.hide();
                false
            }
            BareKey::Enter => {
                if let Some(s) = self.scored.get(self.selected) {
                    let tab_id = s.tab.tab_id;
                    // pos is usize; both +1 and the u32 conversion are
                    // fallible.
                    if let Some(p) = s
                        .tab
                        .position
                        .checked_add(1)
                        .and_then(|n| u32::try_from(n).ok())
                    {
                        switch_tab_to(p);
                        self.record_access(tab_id);
                    }
                }
                self.hide();
                false
            }
            BareKey::Up => {
                self.move_selection(Direction::Up);
                true
            }
            BareKey::Down | BareKey::Tab => {
                self.move_selection(Direction::Down);
                true
            }
            BareKey::Char('p' | 'k') if has_ctrl => {
                self.move_selection(Direction::Up);
                true
            }
            BareKey::Char('n' | 'j') if has_ctrl => {
                self.move_selection(Direction::Down);
                true
            }
            BareKey::Backspace => {
                self.query.pop();
                self.query_changed();
                true
            }
            BareKey::Char('w') if has_ctrl => {
                self.delete_word_backward();
                true
            }
            BareKey::Char('u') if has_ctrl => {
                self.query.clear();
                self.query_changed();
                true
            }
            BareKey::Char(c)
                if !has_ctrl && !has_alt && c.is_ascii_digit() && self.is_jump_pending_for(c) =>
            {
                self.handle_jump_digit(c);
                true
            }
            BareKey::Char(c) if !has_ctrl && !has_alt => {
                // Ignore control chars (e.g. via paste) — prevents SGR
                // injection. Hard cap: keeps long pastes from blowing up
                // the prompt layout.
                if c.is_control() || self.query.chars().count() >= MAX_QUERY_CHARS {
                    return false;
                }
                self.query.push(c);
                self.query_changed();
                true
            }
            _ => false,
        }
    }

    fn move_selection(&mut self, dir: Direction) {
        let len = self.scored.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = match dir {
            Direction::Down => (self.selected + 1) % len,
            Direction::Up => {
                if self.selected == 0 {
                    len - 1
                } else {
                    self.selected - 1
                }
            }
        };
    }

    fn refresh_scores(&mut self) {
        if self.query.is_empty() {
            // Frequency sort. The active tab is filtered out (header).
            // Reuse the existing `Vec` via clear+extend so its capacity
            // survives across refreshes — avoids a fresh allocation per
            // `TabUpdate`.
            let counts = &self.access_counts;
            self.scored.clear();
            self.scored
                .extend(self.tabs.iter().filter(|e| !e.info.active).map(|e| Scored {
                    score: 0,
                    tab: Rc::clone(&e.info),
                    indices: Vec::new(),
                }));
            self.scored.sort_by(|a, b| {
                let ca = counts.get(&a.tab.tab_id).copied().unwrap_or(0);
                let cb = counts.get(&b.tab.tab_id).copied().unwrap_or(0);
                cb.cmp(&ca)
                    .then_with(|| a.tab.position.cmp(&b.tab.position))
            });
            self.clamp_selection();
            return;
        }

        let pattern = Pattern::parse(
            self.query.as_str(),
            CaseMatching::Smart,
            Normalization::Smart,
        );

        self.scored.clear();
        let mut buf: Vec<u32> = Vec::new();
        for e in &self.tabs {
            if e.info.active {
                continue;
            }
            buf.clear();
            if let Some(score) = pattern.indices(e.haystack.slice(..), &mut self.matcher, &mut buf)
            {
                buf.sort_unstable();
                self.scored.push(Scored {
                    score,
                    tab: Rc::clone(&e.info),
                    indices: buf.clone(),
                });
            }
        }
        self.scored.sort_by_key(|s| Reverse(s.score));
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.scored.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.scored.len() {
            self.selected = self.scored.len() - 1;
        }
    }
}

fn render_active_row(out: &mut String, tab: &TabInfo, cols: usize) {
    let raw_panes = tab.selectable_tiled_panes_count + tab.selectable_floating_panes_count;
    let panes = raw_panes.saturating_sub(1);
    // pane_str renders as "[N]" — visible width = digit_count(N) + 2.
    // Computed by formula instead of format-then-measure to skip an alloc.
    let pane_len = digit_count(panes) + 2;
    // Layout: name + gap + "  " (2-space separator) + pane_str
    let name_area = cols.saturating_sub(2 + pane_len);

    // Defensive: on extremely narrow panes, show only the truncated name
    // to avoid overflow.
    if name_area == 0 {
        out.push_str(ACTIVE);
        let written = push_truncated_name(out, &tab.name, &[], cols);
        out.push_str(RESET);
        push_pad(out, cols.saturating_sub(written));
        return;
    }

    out.push_str(ACTIVE);
    let name_written = push_truncated_name(out, &tab.name, &[], name_area);
    out.push_str(RESET);
    out.push_str(DIM);
    out.push_str(pad(name_area.saturating_sub(name_written)));
    let _ = write!(out, "  [{panes}]");
    out.push_str(RESET);
}

fn render_row(
    out: &mut String,
    s: &Scored,
    selected: bool,
    cols: usize,
    pos_width: usize,
    blink_on: bool,
) {
    let tab = &*s.tab;
    // num_prefix renders as "{pos:>pos_width$} - " — visible width is
    // pos_width + 3 (the trailing " - "). pane_str renders as "[N]" with
    // visible width digit_count(N) + 2. Both widths are derived by formula
    // to skip per-row format-then-measure allocations.
    let num_prefix_len = pos_width + 3;
    let panes = tab.selectable_tiled_panes_count + tab.selectable_floating_panes_count;
    let pane_len = digit_count(panes) + 2;
    // Layout: num_prefix + name + gap + "  " (2-space separator) + pane_str
    let name_area = cols.saturating_sub(num_prefix_len + 2 + pane_len);

    // Defensive: on extremely narrow panes num_prefix and pane_str
    // together exceed content_width. Render only the name, no overflow.
    if name_area == 0 {
        let written = push_truncated_name(out, &tab.name, &[], cols);
        push_pad(out, cols.saturating_sub(written));
        return;
    }

    let bell = tab.has_bell_notification;
    let lit = if bell { blink_on } else { true };
    // Highlighted inner: outer style (selected REVERSE or bell bg) sits
    // on top → inner stays plain. Otherwise (no highlight): DIM inner.
    let highlighted = lit && (selected || bell);
    let pos = tab.position + 1;

    if highlighted {
        // Match indices only when not selected — REVERSE would mask them.
        let indices: &[u32] = if selected { &[] } else { &s.indices };
        let _ = write!(out, "{pos:>pos_width$} - ");
        let name_written = push_truncated_name(out, &tab.name, indices, name_area);
        out.push_str(pad(name_area.saturating_sub(name_written)));
        let _ = write!(out, "  [{panes}]");
    } else {
        out.push_str(DIM);
        let _ = write!(out, "{pos:>pos_width$} - ");
        out.push_str(RESET);
        let name_written = push_truncated_name(out, &tab.name, &s.indices, name_area);
        out.push_str(pad(name_area.saturating_sub(name_written)));
        out.push_str("  ");
        out.push_str(DIM);
        let _ = write!(out, "[{panes}]");
        out.push_str(RESET);
    }
}

/// Writes the (possibly truncated) tab name directly into `out`, with
/// match highlighting at every position in `indices` (must be sorted
/// ascending). Truncation and the return value are measured in display
/// cells (UAX #11), not codepoints, so wide chars (emoji, CJK) do not
/// break frame alignment. Returns the number of visible cells written.
fn push_truncated_name(out: &mut String, name: &str, indices: &[u32], max_cells: usize) -> usize {
    if max_cells == 0 {
        return 0;
    }
    // `sanitize_char` maps control chars to '?' (width 1) before display, so
    // measure the sanitized width to match what is actually written.
    let total: usize = name.chars().map(|c| char_cells(sanitize_char(c))).sum();
    if total <= max_cells {
        return push_name_chars(out, name, indices, max_cells);
    }
    if max_cells == 1 {
        out.push('…');
        return 1;
    }
    // Reserve one cell for the trailing ellipsis.
    let written = push_name_chars(out, name, indices, max_cells - 1);
    out.push('…');
    written + 1
}

/// Writes name chars (sanitized, with match highlight at `indices`) until the
/// next char would exceed `budget` display cells. Returns cells written.
fn push_name_chars(out: &mut String, name: &str, indices: &[u32], budget: usize) -> usize {
    let mut used = 0usize;
    let mut idx_iter = indices.iter().peekable();
    for (i, c) in name.chars().enumerate() {
        // Indices come from nucleo as codepoint positions, so `i` is the
        // codepoint index. Should a name exceed u32::MAX chars we break out
        // rather than truncate silently via `as u32`.
        let Ok(i_u32) = u32::try_from(i) else { break };
        // Strip control chars / ANSI escapes so they cannot break our SGR
        // state, then measure the resulting glyph's width.
        let c = sanitize_char(c);
        let w = char_cells(c);
        if used + w > budget {
            break;
        }
        used += w;
        if idx_iter.peek().is_some_and(|&&v| v == i_u32) {
            idx_iter.next();
            out.push_str(MATCH);
            out.push(c);
            out.push_str(RESET);
        } else {
            out.push(c);
        }
    }
    used
}

/// Display width of a single char in terminal cells (UAX #11). Width-`None`
/// chars (controls) are treated as 0; callers sanitize controls beforehand.
fn char_cells(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Replaces control chars (incl. ESC) with '?' — prevents SGR/cursor
/// injection via tab names or query input.
fn sanitize_char(c: char) -> char {
    if c.is_control() {
        '?'
    } else {
        c
    }
}

/// True when `query` is a valid pending-quickjump buffer: non-empty,
/// only ASCII digits, and not starting with `0`.
fn is_pending_jump_buffer(query: &str) -> bool {
    !query.is_empty() && !query.starts_with('0') && query.chars().all(|c| c.is_ascii_digit())
}

/// Number of digits in a number — for pre-computing count-display widths.
fn digit_count(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        n.ilog10() as usize + 1
    }
}

/// Returns the tail of the query that fits in `max_cells` display cells.
/// While typing, the cursor is at the end — that is what the user wants to
/// see — so we keep the most recent characters. Measured in cells (UAX #11)
/// so wide chars in a pasted query do not overrun the prompt row.
fn query_tail(query: &str, max_cells: usize) -> &str {
    if max_cells == 0 {
        return "";
    }
    let mut width = 0usize;
    let mut start = query.len();
    for (idx, c) in query.char_indices().rev() {
        let w = char_cells(c);
        if width + w > max_cells {
            break;
        }
        width += w;
        start = idx;
    }
    &query[start..]
}

/// Counts visible cells in `s`, skipping CSI escape sequences. Each
/// non-escape codepoint contributes its Unicode display width (UAX #11):
/// wide chars (emoji, CJK) count as 2, combining marks as 0. This keeps
/// frame padding aligned even when tab names contain such characters.
fn visible_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if ch == '\u{1b}' {
            in_esc = true;
            continue;
        }
        if in_esc {
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
            continue;
        }
        len += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_count_boundaries() {
        assert_eq!(digit_count(0), 1);
        assert_eq!(digit_count(9), 1);
        assert_eq!(digit_count(10), 2);
        assert_eq!(digit_count(99), 2);
        assert_eq!(digit_count(100), 3);
        assert_eq!(digit_count(1000), 4);
    }

    #[test]
    fn pending_jump_buffer_rules() {
        assert!(!is_pending_jump_buffer("")); // empty
        assert!(!is_pending_jump_buffer("0")); // leading zero
        assert!(!is_pending_jump_buffer("01"));
        assert!(!is_pending_jump_buffer("1a")); // non-digit
        assert!(!is_pending_jump_buffer("abc"));
        assert!(is_pending_jump_buffer("1"));
        assert!(is_pending_jump_buffer("12"));
        assert!(is_pending_jump_buffer("905"));
    }

    #[test]
    fn char_cells_widths() {
        assert_eq!(char_cells('a'), 1);
        assert_eq!(char_cells('世'), 2); // CJK wide
        assert_eq!(char_cells('❯'), 1); // prompt glyph — build_prompt assumes 1
        assert_eq!(char_cells('…'), 1); // ellipsis
        assert_eq!(char_cells('│'), 1); // box drawing
    }

    #[test]
    fn visible_len_skips_escapes_and_counts_width() {
        assert_eq!(visible_len("abc"), 3);
        assert_eq!(visible_len("\u{1b}[1;33mX\u{1b}[0m"), 1); // SGR is zero-width
        assert_eq!(visible_len("世界"), 4); // two CJK = four cells
        assert_eq!(visible_len("a世"), 3);
    }

    #[test]
    fn query_tail_keeps_end_by_width() {
        assert_eq!(query_tail("hello", 0), "");
        assert_eq!(query_tail("hello", 10), "hello"); // fits whole
        assert_eq!(query_tail("hello", 3), "llo"); // most recent chars
        assert_eq!(query_tail("世界", 4), "世界");
        assert_eq!(query_tail("世界", 3), "界"); // only the last wide char fits
        assert_eq!(query_tail("世界", 1), ""); // a 2-wide char cannot fit in 1
    }

    #[test]
    fn sanitize_replaces_controls() {
        assert_eq!(sanitize_char('a'), 'a');
        assert_eq!(sanitize_char('世'), '世');
        assert_eq!(sanitize_char('\u{1b}'), '?'); // ESC
        assert_eq!(sanitize_char('\n'), '?');
    }

    #[test]
    fn truncated_name_fits_returns_width() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[], 10);
        assert_eq!(s, "abc");
        assert_eq!(w, 3);
    }

    #[test]
    fn truncated_name_adds_ellipsis() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abcdef", &[], 4);
        assert_eq!(s, "abc…");
        assert_eq!(w, 4);
    }

    #[test]
    fn truncated_name_wide_chars_respect_cells() {
        // "世界世" = 6 cells; budget 5 reserves 1 for '…', fits 4 cells of name.
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "世界世", &[], 5);
        assert_eq!(s, "世界…");
        assert_eq!(w, 5);
    }

    #[test]
    fn truncated_name_wide_char_partial_cell() {
        // Budget 4 → content budget 3, next char is 2-wide → only "世" fits.
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "世界世", &[], 4);
        assert_eq!(s, "世…");
        assert_eq!(w, 3);
    }

    #[test]
    fn truncated_name_single_cell_is_ellipsis() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[], 1);
        assert_eq!(s, "…");
        assert_eq!(w, 1);
    }

    #[test]
    fn truncated_name_zero_budget_writes_nothing() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[], 0);
        assert_eq!(s, "");
        assert_eq!(w, 0);
    }

    #[test]
    fn truncated_name_highlights_indices() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[1], 10);
        assert_eq!(s, format!("a{MATCH}b{RESET}c")); // escapes are zero-width
        assert_eq!(w, 3);
    }

    #[test]
    fn pad_and_push_pad() {
        assert_eq!(pad(3), "   ");
        assert_eq!(pad(0), "");
        // pad() is capped at PAD_SPACES length; push_pad must loop past it.
        let mut s = String::new();
        push_pad(&mut s, 300);
        assert_eq!(s.len(), 300);
        assert!(s.bytes().all(|b| b == b' '));
    }
}
