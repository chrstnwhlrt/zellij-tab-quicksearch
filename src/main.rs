use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write;
use std::rc::Rc;
use zellij_tile::prelude::*;

use nucleo_matcher::chars::to_lower_case;
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
// Re-establishes `BELL` after a `MATCH` run inside a bell row. A bare
// `RESET` there would drop the row background for everything after the
// first highlighted char; `0` resets, then the bell attributes re-apply.
const BELL_RESTORE: &str = "\u{1b}[0;7;38;5;250m";

// Static padding to avoid `" ".repeat(n)` allocations in hot paths.
const PAD_SPACES: &str = "                                                                                                                                ";

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

// Smallest box the frame layout can render (frame + minimal content).
// Configured sizes are clamped to it so `max_cols "0"` cannot produce an
// invisible picker; smaller *panes* still render blank.
const MIN_BOX_COLS: usize = 20;
const MIN_BOX_ROWS: usize = 4;

// Blink interval for the bell background (toggle per tick).
const BLINK_INTERVAL: f64 = 0.5;

// Time after the (D-1)-th digit before a quickjump fires on the buffered
// number. Picked to be long enough for the user to type the final digit
// of a known multi-digit position, but short enough to feel responsive
// for a deliberate single-digit jump.
const JUMP_TIMEOUT: f64 = 0.4;

// Tolerance when attributing a `Timer(elapsed)` event to the requested
// duration of a pending timer: covers float rounding only, the host never
// fires a timer early.
const TIMER_SLACK: f64 = 0.01;

// Upper bound for the timer bookkeeping queue. Only reachable if the host
// ever drops a `Timer` event; the oldest entry is then assumed lost.
const MAX_PENDING_TIMERS: usize = 8;

// Characters with a special meaning in nucleo's query syntax (`!` negation,
// `^` prefix, `$` suffix, `'` substring, `\` escape). Queries using them get
// nucleo's exact semantics only — no typo-tolerant fallback that could
// contradict e.g. a negation.
const NUCLEO_OPERATORS: &str = "!^$'\\";

// Tab names longer than this skip the typo-tolerant pass (O(query × name)).
const APPROX_MAX_NAME_CHARS: usize = 512;

#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

struct TabEntry {
    info: Rc<TabInfo>,
    /// nucleo haystack, indexed by codepoint (see `haystack_for`).
    haystack: Utf32String,
    /// Plain codepoints of the name, for the typo-tolerant pass.
    chars: Vec<char>,
}

/// How a tab matched the query. Every nucleo (subsequence) match ranks
/// above every approximate (typo-tolerant) one, see `rank`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchKind {
    /// No query: the unfiltered list, ordered by access frequency (see
    /// `State::list_by_frequency`), never by `rank`.
    Listed,
    /// nucleo fuzzy match with its score (higher is better).
    Fuzzy(u32),
    /// Approximate substring match with its edit distance (lower is better).
    Approx(u32),
}

impl MatchKind {
    /// Sort key, best match first. A stable sort keeps tab order for ties.
    fn rank(self) -> (u8, u64) {
        match self {
            MatchKind::Listed => (0, 0),
            MatchKind::Fuzzy(score) => (0, u64::from(u32::MAX - score)),
            MatchKind::Approx(distance) => (1, u64::from(distance)),
        }
    }
}

struct Scored {
    kind: MatchKind,
    tab: Rc<TabInfo>,
    /// Codepoint positions in the tab name to highlight, ascending, unique.
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
            cols: parse(cfg, "max_cols", d.cols).max(MIN_BOX_COLS),
            rows: parse(cfg, "max_rows", d.rows).max(MIN_BOX_ROWS),
        }
    }
}

/// Plugin visibility, derived from independent indicators (see
/// `is_visible`):
///
/// - `pane_focused` (`PaneUpdate`): plugin pane is in the floating layer
///   and focused.
/// - `float_visible` (`TabUpdate.are_floating_panes_visible`): the
///   floating layer is shown in the user's active tab. Flips to `false`
///   when the user clicks into the tile below without changing
///   `pane_focused` — so `pane_focused` alone is not sufficient.
/// - `host_visible` (`Event::Visible`): the host's own last word, if it
///   gave one. Zellij 0.44.x/0.45.0 never send it to floating panes; the
///   fix on Zellij's main branch (after 0.45.0) sends `false` when the
///   plugin's tab is switched away, the floating layer is hidden or the
///   client detaches, and `true` on return. Neither version sends it for
///   `hide_self`/reopen. Cleared by every `TabUpdate`/`PaneUpdate`, which
///   only reach a plugin whose tab is the active one — so a missing
///   `Visible(true)` (e.g. after the pane moved to another tab) can never
///   leave the picker stuck invisible.
///
/// Known limitation: a hidden plugin (`hide_self`) receives no
/// `TabUpdate`/`PaneUpdate` at all — Zellij targets only the tiled and
/// floating panes of the active tab. `hide()` therefore resets these flags
/// locally, and the reopen is detected as the rising edge of
/// `pane_focused` in `PaneUpdate`. Without `Visible` events the blink loop
/// keeps running invisibly after a tab switch (not perceivable,
/// CPU-irrelevant) and terminates on return via `TabUpdate`.
#[derive(Default)]
struct Visibility {
    visible: bool,
    pane_focused: bool,
    float_visible: bool,
    host_visible: Option<bool>,
}

impl Visibility {
    /// Visible when both local indicators say so and the host did not
    /// explicitly say otherwise.
    fn is_visible(&self) -> bool {
        self.pane_focused && self.float_visible && self.host_visible != Some(false)
    }
}

/// Where the tab list was drawn in the last render, in pane-relative cells
/// (the coordinate system of Zellij's `Mouse` events). `None` while the
/// picker is not drawn (not ready, or the pane is too small).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ListArea {
    top: usize,
    rows: usize,
    left: usize,
    width: usize,
}

/// What a pending host timer is for. zellij-tile has no cancel API and
/// `Event::Timer` carries no id, only the elapsed time, so the plugin keeps
/// its own bookkeeping to tell the two timer sources apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerKind {
    /// Bell-blink tick. At most one is ever in flight.
    Blink,
    /// Quickjump timeout. Only the newest generation may act.
    Jump { generation: u32 },
}

#[derive(Default)]
struct Timers {
    /// Toggles per blink tick — drives the bell-row background colour.
    bell_blink_on: bool,
    /// In-flight host timers, oldest first, with their requested duration.
    pending: VecDeque<(TimerKind, f64)>,
    /// Bumped by every new jump timer and by `State::hide`; a `Jump` entry
    /// with an older generation is void (superseded, or the picker closed).
    jump_generation: u32,
}

impl Timers {
    fn blink_in_flight(&self) -> bool {
        self.pending
            .iter()
            .any(|(kind, _)| *kind == TimerKind::Blink)
    }

    /// Records a timer that was just requested from the host. At capacity
    /// the oldest non-blink entry is dropped first, so the blink singleton
    /// survives even a burst of superseded jump timers.
    fn armed(&mut self, kind: TimerKind, secs: f64) {
        if self.pending.len() >= MAX_PENDING_TIMERS {
            let victim = self
                .pending
                .iter()
                .position(|(k, _)| *k != TimerKind::Blink)
                .unwrap_or(0);
            self.pending.remove(victim);
        }
        self.pending.push_back((kind, secs));
    }

    /// Attributes a `Timer(elapsed)` event to a pending timer and removes
    /// it: the oldest entry whose requested duration has elapsed. Host
    /// timers fire in deadline order and are delivered in order, so that is
    /// the one that fired; only a late-delivered event can be attributed to
    /// a sibling of equal duration, which merely swaps two handlers. Falls
    /// back to the oldest entry when none is eligible.
    fn resolve(&mut self, elapsed: f64) -> Option<TimerKind> {
        let idx = self
            .pending
            .iter()
            .position(|(_, secs)| *secs <= elapsed + TIMER_SLACK)
            .unwrap_or(0);
        self.pending.remove(idx).map(|(kind, _)| kind)
    }

    /// Starts a new quickjump generation, voiding every pending jump timer.
    fn new_jump_generation(&mut self) -> u32 {
        self.jump_generation = self.jump_generation.wrapping_add(1);
        self.jump_generation
    }
}

#[derive(Default)]
struct State {
    tabs: Vec<TabEntry>,
    /// True once the first `TabUpdate` arrived — before that an empty list
    /// means "not yet known", not "no tabs".
    tabs_received: bool,
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
    /// List geometry of the last render, for mouse hit-testing.
    list_area: Option<ListArea>,
    /// Access frequency via the picker (Enter / instant-jump / click).
    /// Most-used tab first. The active tab is filtered out before sorting
    /// and shown as a header. Pruned to live tabs on each `TabUpdate`.
    access_counts: BTreeMap<usize, u64>,
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
            // `PaneUpdate`: geometry re-assert (reopen, layout resets), plus
            // the `pane_focused` visibility indicator (see `Visibility`).
            EventType::PaneUpdate,
            EventType::Key,
            // `Mouse`: click to select / confirm, wheel to move the selection.
            EventType::Mouse,
            // `Timer`: quickjump buffer + bell-blink loop.
            EventType::Timer,
            // `Visible`: the host's visibility signal, where it sends one.
            EventType::Visible,
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
                // handler re-asserts the geometry on every reopen and on
                // later layout resets, so this is a one-time fast-path.
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
            Event::TabUpdate(new_tabs) => self.on_tab_update(new_tabs),
            Event::PaneUpdate(manifest) => self.on_pane_update(&manifest),
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
            Event::Mouse(mouse) => {
                if !self.ready {
                    return false;
                }
                self.handle_mouse(&mouse)
            }
            Event::Timer(elapsed) => self.on_timer(elapsed),
            Event::Visible(shown) => self.on_visible(shown),
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        // One string buffer, one print! → one syscall instead of ~30.
        let mut out = String::with_capacity(rows * (cols + 8));

        if !self.ready {
            self.list_area = None;
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
        // render area equals the box → no padding. Otherwise (the frame
        // between (re)open and the resize taking effect) the box is
        // rendered compact with padding around it — cosmetically imperfect
        // but stable.
        let place = BoxPlacement::centered(rows, cols, &self.size);
        for _ in 0..place.vpad_top {
            append_padded(&mut out, cols, "");
        }
        self.render_box(&mut out, place);
        for _ in 0..place.vpad_bottom(rows) {
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
    push_pad(out, gap);
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

/// Where the box sits inside the pane: `rows` × `cols` cells, starting at
/// column `hpad` below `vpad_top` blank rows, in a pane `outer_cols` wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoxPlacement {
    rows: usize,
    cols: usize,
    hpad: usize,
    vpad_top: usize,
    outer_cols: usize,
}

impl BoxPlacement {
    /// Centres a box capped at `size` inside a `rows` × `cols` pane.
    fn centered(rows: usize, cols: usize, size: &SizeCfg) -> Self {
        let box_cols = cols.min(size.cols);
        let box_rows = rows.min(size.rows);
        Self {
            rows: box_rows,
            cols: box_cols,
            hpad: cols.saturating_sub(box_cols) / 2,
            vpad_top: rows.saturating_sub(box_rows) / 2,
            outer_cols: cols,
        }
    }

    /// Blank pane rows below the box.
    fn vpad_bottom(self, pane_rows: usize) -> usize {
        pane_rows
            .saturating_sub(self.rows)
            .saturating_sub(self.vpad_top)
    }

    fn row_frame(self) -> RowFrame {
        RowFrame {
            hpad: self.hpad,
            cols: self.cols,
            trail: self.outer_cols.saturating_sub(self.hpad + self.cols),
        }
    }
}

/// Horizontal layout shared by every box row: `hpad` blank cells, the box
/// (`cols` wide, six of which are frame and padding), then `trail` blank
/// cells up to the pane's right edge — every row is exactly one pane wide,
/// so no cell is left transparent.
#[derive(Clone, Copy)]
struct RowFrame {
    hpad: usize,
    cols: usize,
    trail: usize,
}

impl RowFrame {
    /// Cells available for row content between the frame paddings.
    fn content_width(self) -> usize {
        self.cols.saturating_sub(6)
    }

    /// Writes a horizontal border row (╭─╮ / ├─┤ / ╰─╯).
    fn border(self, out: &mut String, left: char, right: char) {
        push_pad(out, self.hpad);
        append_horizontal_border(out, left, right, self.cols);
        push_pad(out, self.trail);
        out.push('\n');
    }

    /// Writes a framed content row (│ content │).
    fn wrapped(self, out: &mut String, content: &str, style: HighlightStyle) {
        push_pad(out, self.hpad);
        append_wrap(out, content, self.content_width(), style);
        push_pad(out, self.trail);
        out.push('\n');
    }
}

impl State {
    fn build_prompt(&self, content: &mut String, content_width: usize) {
        // Layout: "❯ " + query + cursor block. Padding to the box edge is
        // handled by append_wrap; `query_tail` keeps the most recent chars
        // that fit, measured in display cells like everything else.
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
        frame: RowFrame,
    ) {
        let content_width = frame.content_width();
        if self.scored.is_empty() {
            // Before the first `TabUpdate` the list is merely unknown, not
            // empty — render blank rows instead of a misleading message.
            let msg = match (self.tabs_received, self.tabs.is_empty()) {
                (false, _) => "",
                (true, true) => "no tabs",
                (true, false) => "no matches",
            };
            let lpad = content_width.saturating_sub(msg.len()) / 2;
            let mid = list_rows / 2;
            for row in 0..list_rows {
                content.clear();
                if row == mid && !msg.is_empty() {
                    push_pad(content, lpad);
                    content.push_str(DIM);
                    content.push_str(msg);
                    content.push_str(RESET);
                }
                frame.wrapped(out, content, HighlightStyle::default());
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
            frame.wrapped(out, content, style);
        }
    }

    /// Draws the box at `place` and records where the list rows ended up
    /// (`list_area`) for mouse hit-testing.
    fn render_box(&mut self, out: &mut String, place: BoxPlacement) {
        // Minimum size: frame(6) + minimum content. Below that render blank.
        if place.rows < MIN_BOX_ROWS || place.cols < MIN_BOX_COLS {
            self.list_area = None;
            for _ in 0..place.rows {
                append_padded(out, place.outer_cols, "");
            }
            return;
        }
        let frame = place.row_frame();
        let content_width = frame.content_width();
        let active_tab = self
            .tabs
            .iter()
            .find(|e| e.info.active)
            .map(|e| Rc::clone(&e.info));
        let mut content = String::with_capacity(place.cols * 2);

        frame.border(out, '╭', '╮');
        self.build_prompt(&mut content, content_width);
        frame.wrapped(out, &content, HighlightStyle::default());
        frame.border(out, '├', '┤');

        // Rows above the list: top border, prompt, separator (+ active-tab
        // header and its separator when shown).
        let mut header_rows = 3;
        let mut list_rows = place.rows.saturating_sub(4);
        if let Some(t) = &active_tab {
            if list_rows >= 2 {
                content.clear();
                render_active_row(&mut content, t, content_width);
                frame.wrapped(out, &content, HighlightStyle::default());
                frame.border(out, '├', '┤');
                list_rows -= 2;
                header_rows += 2;
            }
        }

        self.list_area = Some(ListArea {
            top: place.vpad_top + header_rows,
            rows: list_rows,
            left: place.hpad,
            width: place.cols,
        });
        self.render_list(out, &mut content, list_rows, frame);
        frame.border(out, '╰', '╯');
    }

    /// `TabUpdate`: refreshes the tab cache, the scores and visibility.
    fn on_tab_update(&mut self, new_tabs: Vec<TabInfo>) -> bool {
        // Visibility first, ahead of the dedup below. `hide()` resets the
        // local flags while `self.tabs` keeps the pre-hide snapshot, and a
        // hidden plugin receives no updates at all, so the `TabUpdate` that
        // arrives on reopen can be identical to that snapshot. Skipping it
        // must not leave `float_visible` stale. (`are_floating_panes_visible`
        // of the active tab co-determines visibility: a click into the
        // background pane hides the floating layer without changing
        // `is_focused`.) Any update proves our tab is the active one, which
        // retires a host-side `Visible(false)`.
        self.vis.host_visible = None;
        self.vis.float_visible = new_tabs
            .iter()
            .find(|t| t.active)
            .is_some_and(|t| t.are_floating_panes_visible);
        // Dedup: Zellij fires `TabUpdate` often without changes. `PartialEq`
        // on `TabInfo` covers all fields, including `has_bell_notification`,
        // so only the cheap visibility bookkeeping runs for a repeat.
        let unchanged = self.tabs_received
            && self.tabs.len() == new_tabs.len()
            && new_tabs.iter().zip(&self.tabs).all(|(n, e)| *n == *e.info);
        if unchanged {
            let became_visible = self.recompute_visibility();
            self.start_blink_if_needed();
            return became_visible;
        }
        self.tabs_received = true;
        // Cache the fuzzy haystacks once per `TabUpdate` (not per keystroke
        // × tab).
        self.tabs = new_tabs
            .into_iter()
            .map(|t| TabEntry {
                haystack: haystack_for(&t.name),
                chars: t.name.chars().collect(),
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
        // A newly arriving bell may need to start the loop even without a
        // visibility transition — `start_blink_if_needed` is idempotent.
        self.start_blink_if_needed();
        true
    }

    /// `PaneUpdate`: derives the `pane_focused` visibility indicator and
    /// re-asserts the pane geometry when Zellij changed it.
    fn on_pane_update(&mut self, manifest: &PaneManifest) -> bool {
        if !self.ready {
            return false;
        }
        // Updates only reach plugins in the active tab — see `Visibility`.
        self.vis.host_visible = None;
        let pid = self.plugin_id;
        let own_pane = manifest.panes.iter().find_map(|(tab_position, panes)| {
            panes
                .iter()
                .find(|p| p.is_plugin && p.id == pid)
                .map(|p| (*tab_position, p))
        });
        let Some((tab_position, pane)) = own_pane else {
            // Not in the manifest (e.g. mid-move): treat as unfocused.
            self.vis.pane_focused = false;
            return self.recompute_visibility();
        };
        let pane_focused_now = pane.is_floating && !pane.is_suppressed && pane.is_focused;
        // Every reopen re-inserts the floating pane at Zellij's default
        // geometry (half the viewport, with a frame content offset):
        // `hide_self` takes it out of the floating layer and
        // `LaunchOrFocusPlugin` adds it back as if new. A hidden plugin gets
        // no updates, so the rising edge of `pane_focused` is the first
        // signal that the pane is back — re-assert unconditionally there.
        let reappeared = pane_focused_now && !self.vis.pane_focused;
        if pane.is_floating
            && (reappeared
                || self.geometry_needs_reassert(tab_position, pane.pane_columns, pane.pane_rows))
        {
            self.resize_pane();
        }
        self.vis.pane_focused = pane_focused_now;
        self.recompute_visibility()
    }

    /// Whether the floating pane's current geometry warrants re-sending the
    /// target size. Larger than target: always — `viewport >= pane > target`,
    /// so the request succeeds. Smaller than target: only when the target
    /// fits the tab's viewport; Zellij centres a width/height-only request,
    /// so "fits" means it succeeds, while a pane that is small because the
    /// terminal is small is left alone (render caps the box to the pane).
    /// This can never storm: a floating-pane resize emits no `PaneUpdate`
    /// (Zellij only schedules a repaint), so nothing feeds back.
    fn geometry_needs_reassert(&self, tab_position: usize, cols: usize, rows: usize) -> bool {
        let (target_cols, target_rows) = (self.size.cols, self.size.rows);
        if cols == target_cols && rows == target_rows {
            return false;
        }
        if cols > target_cols || rows > target_rows {
            return true;
        }
        self.tabs
            .iter()
            .find(|e| e.info.position == tab_position)
            .is_some_and(|e| {
                target_cols <= e.info.viewport_columns && target_rows <= e.info.viewport_rows
            })
    }

    /// `Timer`: dispatches to the quickjump or the blink handler.
    fn on_timer(&mut self, elapsed: f64) -> bool {
        match self.timers.resolve(elapsed) {
            Some(TimerKind::Jump { generation }) => self.on_jump_timer(generation),
            Some(TimerKind::Blink) => self.on_blink_timer(),
            None => false,
        }
    }

    /// Quickjump timeout after the (D-1)-th digit: jump if the query is
    /// still that digit buffer and this timer is the newest one.
    fn on_jump_timer(&mut self, generation: u32) -> bool {
        // Superseded by a later digit, or the picker was closed since.
        if generation != self.timers.jump_generation {
            return false;
        }
        if !self.ready || !self.vis.visible || !is_pending_jump_buffer(&self.query) {
            return false;
        }
        let Ok(pos) = self.query.parse::<u32>() else {
            return false;
        };
        // On success the picker is hidden; render once more so the pane's
        // stored grid holds the cleared state.
        self.try_instant_jump(pos)
    }

    /// Bell-blink tick: toggles the background and re-arms while the
    /// plugin is visible and a bell tab exists; otherwise the loop ends.
    fn on_blink_timer(&mut self) -> bool {
        if self.vis.visible && self.has_bell_tab() {
            self.timers.bell_blink_on = !self.timers.bell_blink_on;
            self.arm_timer(TimerKind::Blink, BLINK_INTERVAL);
            true
        } else {
            // Conditions gone — loop terminates. Restarted by
            // `recompute_visibility` (`became_visible`) or `TabUpdate` as
            // soon as conditions hold again.
            self.timers.bell_blink_on = false;
            false
        }
    }

    /// True when at least one tab has a bell notification.
    fn has_bell_tab(&self) -> bool {
        self.tabs.iter().any(|e| e.info.has_bell_notification)
    }

    /// `Visible`: the host's visibility signal (see `Visibility`). A
    /// `false` stops the blink loop on its next tick and parks the
    /// quickjump timeout; a `true` re-renders and restarts the loop.
    fn on_visible(&mut self, shown: bool) -> bool {
        self.vis.host_visible = Some(shown);
        self.recompute_visibility()
    }

    /// Recomputes `visible` from the independent indicators and starts the
    /// blink loop on a `false → true` transition; returns whether that
    /// transition happened (callers re-render on it). The `true → false`
    /// transition needs no action of its own: each timer tick checks at
    /// schedule time whether it should still run.
    fn recompute_visibility(&mut self) -> bool {
        let new_visible = self.vis.is_visible();
        let became_visible = !self.vis.visible && new_visible;
        self.vis.visible = new_visible;
        if became_visible {
            self.start_blink_if_needed();
        }
        became_visible
    }

    /// Starts the bell-blink tick when (a) the plugin is visible, (b) a
    /// bell tab exists, and (c) no tick is already in flight.
    fn start_blink_if_needed(&mut self) {
        if !self.vis.visible || !self.has_bell_tab() || self.timers.blink_in_flight() {
            return;
        }
        self.arm_timer(TimerKind::Blink, BLINK_INTERVAL);
    }

    /// Requests a host timer and records what it is for.
    fn arm_timer(&mut self, kind: TimerKind, secs: f64) {
        self.timers.armed(kind, secs);
        set_timeout(secs);
    }

    /// Mouse: the wheel moves the selection one row per event without
    /// wrapping (Zellij reports three *lines* per wheel step — scrollback
    /// semantics — which would race through a short list), a left click
    /// selects the row under the cursor, and a click on the already
    /// selected row confirms it (the mouse equivalent of Enter). Everything
    /// else — right clicks, drags, hover, clicks outside the list — is
    /// ignored.
    fn handle_mouse(&mut self, mouse: &Mouse) -> bool {
        match mouse {
            Mouse::ScrollUp(_) => self.move_selection_by(Direction::Up, 1),
            Mouse::ScrollDown(_) => self.move_selection_by(Direction::Down, 1),
            Mouse::LeftClick(line, column) => {
                let Some(idx) = self.list_index_at(*line, *column) else {
                    return false;
                };
                if idx == self.selected {
                    self.jump_to_selected();
                } else {
                    self.selected = idx;
                }
                true
            }
            _ => false,
        }
    }

    /// Maps a pane-relative click position to an index into `scored`, if
    /// it hits a populated list row of the last render.
    fn list_index_at(&self, line: isize, column: usize) -> Option<usize> {
        let area = self.list_area?;
        let line = usize::try_from(line).ok()?;
        let inside_rows = (area.top..area.top + area.rows).contains(&line);
        let inside_cols = (area.left..area.left + area.width).contains(&column);
        if !inside_rows || !inside_cols {
            return None;
        }
        let idx = self.scroll_offset + (line - area.top);
        (idx < self.scored.len()).then_some(idx)
    }

    /// Moves the selection by `count` rows, clamped at both ends (unlike
    /// the keyboard, the wheel does not wrap). Returns whether it moved.
    fn move_selection_by(&mut self, dir: Direction, count: usize) -> bool {
        let Some(last) = self.scored.len().checked_sub(1) else {
            return false;
        };
        let target = match dir {
            Direction::Up => self.selected.saturating_sub(count),
            Direction::Down => self.selected.saturating_add(count).min(last),
        };
        let moved = target != self.selected;
        self.selected = target;
        moved
    }

    /// Switches to the selected tab, if there is one, and closes the
    /// picker.
    fn jump_to_selected(&mut self) {
        if let Some(s) = self.scored.get(self.selected) {
            let tab_id = s.tab.tab_id;
            // pos is usize; both +1 and the u32 conversion are fallible.
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
    }

    /// Sends the target geometry to Zellij. `borderless` travels inside the
    /// coordinates so Zellij applies it together with the geometry (its
    /// `change_pane_coordinates` re-derives the content offset from the
    /// borderless flag right after resizing). The explicit
    /// `set_pane_borderless` additionally covers a pane that is not floating
    /// (e.g. launched with `floating false`).
    fn resize_pane(&self) {
        change_floating_panes_coordinates(vec![(
            PaneId::Plugin(self.plugin_id),
            FloatingPaneCoordinates {
                borderless: Some(true),
                ..FloatingPaneCoordinates::default()
                    .with_width_fixed(self.size.cols)
                    .with_height_fixed(self.size.rows)
            },
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
        // Mark invisible locally right away: `PaneUpdate`/`TabUpdate` would
        // do the same, but a hidden plugin never receives them.
        // `pane_focused = false` also arms the rising-edge detection that
        // re-asserts the geometry on the next open.
        self.vis = Visibility::default();
        // Void pending quickjump timers — zellij-tile has no cancel API, the
        // events still arrive and must no-op. The blink timer, if any, stays
        // queued: it terminates itself on its next tick because `visible`
        // is false, and keeping it queued is exactly what stops
        // `start_blink_if_needed` from arming a second chain on reopen.
        self.timers.new_jump_generation();
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
            // final digit. A fresh generation voids any earlier jump timer
            // (e.g. digit, Backspace, digit), so the timeout always counts
            // from the latest digit.
            let generation = self.timers.new_jump_generation();
            self.arm_timer(TimerKind::Jump { generation }, JUMP_TIMEOUT);
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

        // Every close path returns `true`: one more render after `hide()`
        // leaves the cleared picker in the pane's stored grid (Zellij routes
        // render output to suppressed panes too), so a reopen never flashes
        // the previous query even before the first update arrives.
        match key.bare_key {
            BareKey::Esc => {
                self.hide();
                true
            }
            BareKey::Char('c' | 'g') if has_ctrl => {
                self.hide();
                true
            }
            BareKey::Enter => {
                self.jump_to_selected();
                true
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

    /// Rebuilds `scored` for the current query: the unfiltered list in
    /// frequency order when the query is empty, the search otherwise.
    fn refresh_scores(&mut self) {
        if self.query.is_empty() {
            self.list_by_frequency();
        } else {
            self.search();
        }
        self.clamp_selection();
    }

    /// Empty query: every tab except the active one (that is the header),
    /// most-used first, then by position. Reuses the `scored` allocation
    /// (clear + extend) so its capacity survives across `TabUpdate`s.
    fn list_by_frequency(&mut self) {
        let counts = &self.access_counts;
        self.scored.clear();
        self.scored
            .extend(self.tabs.iter().filter(|e| !e.info.active).map(|e| Scored {
                kind: MatchKind::Listed,
                tab: Rc::clone(&e.info),
                indices: Vec::new(),
            }));
        self.scored.sort_by(|a, b| {
            let ca = counts.get(&a.tab.tab_id).copied().unwrap_or(0);
            let cb = counts.get(&b.tab.tab_id).copied().unwrap_or(0);
            cb.cmp(&ca)
                .then_with(|| a.tab.position.cmp(&b.tab.position))
        });
    }

    /// Non-empty query: nucleo over every tab except the active one, then
    /// the typo-tolerant pass over what nucleo rejected; best match first.
    fn search(&mut self) {
        let pattern = Pattern::parse(
            self.query.as_str(),
            CaseMatching::Smart,
            Normalization::Smart,
        );

        self.scored.clear();
        let mut buf: Vec<u32> = Vec::new();
        // Tabs nucleo rejected — candidates for the typo-tolerant pass.
        let mut unmatched: Vec<usize> = Vec::new();
        for (ti, e) in self.tabs.iter().enumerate() {
            if e.info.active {
                continue;
            }
            buf.clear();
            if let Some(score) = pattern.indices(e.haystack.slice(..), &mut self.matcher, &mut buf)
            {
                // nucleo appends the indices of every pattern atom without
                // sorting or deduplicating them.
                buf.sort_unstable();
                buf.dedup();
                self.scored.push(Scored {
                    kind: MatchKind::Fuzzy(score),
                    tab: Rc::clone(&e.info),
                    indices: buf.clone(),
                });
            } else {
                unmatched.push(ti);
            }
        }
        self.approx_pass(&unmatched);
        // Stable: nucleo matches by score, then approximate ones by
        // distance, ties in tab order.
        self.scored.sort_by_key(|s| s.kind.rank());
    }

    /// Typo-tolerant pass over the tabs nucleo did not match: a bounded
    /// edit-distance substring match (`approx_match`) so that `bakkend` or
    /// `bacckend` still find `backend`. Smart case like nucleo (a query
    /// without uppercase matches case-insensitively); no diacritic folding.
    /// Skipped for very short queries (everything would match) and for
    /// queries using nucleo's operator syntax.
    fn approx_pass(&mut self, candidates: &[usize]) {
        let query: Vec<char> = self.query.chars().collect();
        let max_errors = max_errors(query.len());
        if max_errors == 0 || query.iter().any(|c| NUCLEO_OPERATORS.contains(*c)) {
            return;
        }
        let fold_case = !query.iter().any(|c| c.is_uppercase());
        let mut text: Vec<char> = Vec::new();
        let mut scratch: Vec<u16> = Vec::new();
        for &ti in candidates {
            let e = &self.tabs[ti];
            if e.chars.len() > APPROX_MAX_NAME_CHARS {
                continue;
            }
            text.clear();
            text.extend(
                e.chars
                    .iter()
                    .map(|&c| if fold_case { to_lower_case(c) } else { c }),
            );
            if let Some((distance, indices)) = approx_match(&query, &text, max_errors, &mut scratch)
            {
                self.scored.push(Scored {
                    kind: MatchKind::Approx(distance),
                    tab: Rc::clone(&e.info),
                    indices,
                });
            }
        }
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
        let written = push_truncated_name(out, &tab.name, &[], cols, RESET);
        out.push_str(RESET);
        push_pad(out, cols.saturating_sub(written));
        return;
    }

    out.push_str(ACTIVE);
    let name_written = push_truncated_name(out, &tab.name, &[], name_area, RESET);
    out.push_str(RESET);
    out.push_str(DIM);
    push_pad(out, name_area.saturating_sub(name_written));
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
        let written = push_truncated_name(out, &tab.name, &[], cols, RESET);
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
        // Inside a bell row every match run must restore the bell
        // background, not reset to plain.
        let indices: &[u32] = if selected { &[] } else { &s.indices };
        let _ = write!(out, "{pos:>pos_width$} - ");
        let name_written = push_truncated_name(out, &tab.name, indices, name_area, BELL_RESTORE);
        push_pad(out, name_area.saturating_sub(name_written));
        let _ = write!(out, "  [{panes}]");
    } else {
        out.push_str(DIM);
        let _ = write!(out, "{pos:>pos_width$} - ");
        out.push_str(RESET);
        let name_written = push_truncated_name(out, &tab.name, &s.indices, name_area, RESET);
        push_pad(out, name_area.saturating_sub(name_written));
        out.push_str("  ");
        out.push_str(DIM);
        let _ = write!(out, "[{panes}]");
        out.push_str(RESET);
    }
}

/// Writes the (possibly truncated) tab name directly into `out`, with
/// match highlighting at every position in `indices` (sorted ascending;
/// duplicates are tolerated). `restore` is the SGR emitted after each
/// highlighted char — `RESET` on plain rows, `BELL_RESTORE` inside a bell
/// row. Truncation and the return value are measured in display cells
/// (UAX #11), not codepoints, so wide chars (emoji, CJK) do not break
/// frame alignment. Returns the number of visible cells written.
fn push_truncated_name(
    out: &mut String,
    name: &str,
    indices: &[u32],
    max_cells: usize,
    restore: &str,
) -> usize {
    if max_cells == 0 {
        return 0;
    }
    // `sanitize_char` maps control chars to '?' (width 1) before display, so
    // measure the sanitized width to match what is actually written.
    let total: usize = name.chars().map(|c| char_cells(sanitize_char(c))).sum();
    if total <= max_cells {
        return push_name_chars(out, name, indices, max_cells, restore);
    }
    if max_cells == 1 {
        out.push('…');
        return 1;
    }
    // Reserve one cell for the trailing ellipsis.
    let written = push_name_chars(out, name, indices, max_cells - 1, restore);
    out.push('…');
    written + 1
}

/// Writes name chars (sanitized, with match highlight at `indices`) until the
/// next char would exceed `budget` display cells. Returns cells written.
fn push_name_chars(
    out: &mut String,
    name: &str,
    indices: &[u32],
    budget: usize,
    restore: &str,
) -> usize {
    let mut used = 0usize;
    let mut idx_iter = indices.iter().peekable();
    for (i, c) in name.chars().enumerate() {
        // Indices are codepoint positions, so `i` is the codepoint index.
        // Should a name exceed u32::MAX chars we break out rather than
        // truncate silently via `as u32`.
        let Ok(i_u32) = u32::try_from(i) else { break };
        // Strip control chars / ANSI escapes so they cannot break our SGR
        // state, then measure the resulting glyph's width.
        let c = sanitize_char(c);
        let w = char_cells(c);
        if used + w > budget {
            break;
        }
        used += w;
        // Consume every index up to `i`: duplicates or stale positions must
        // not wedge the iterator and hide later highlights.
        let mut is_match = false;
        while idx_iter.peek().is_some_and(|&&v| v <= i_u32) {
            if let Some(&v) = idx_iter.next() {
                is_match |= v == i_u32;
            }
        }
        if is_match {
            out.push_str(MATCH);
            out.push(c);
            out.push_str(restore);
        } else {
            out.push(c);
        }
    }
    used
}

/// nucleo haystack for a tab name. `Utf32String::from(&str)` collapses
/// grapheme clusters to their first codepoint (lossy), which would desync
/// nucleo's match indices from the codepoint positions `push_name_chars`
/// highlights. Building the `Unicode` variant from the plain codepoints
/// keeps both in step; ASCII names take nucleo's fast path unchanged.
fn haystack_for(name: &str) -> Utf32String {
    if name.is_ascii() {
        Utf32String::Ascii(Box::from(name))
    } else {
        Utf32String::Unicode(name.chars().collect())
    }
}

/// Edit distance tolerated by the typo-tolerant pass for a query of `len`
/// chars: none for one or two chars (everything would match), one typo up
/// to five chars, two up to nine, three beyond.
fn max_errors(len: usize) -> u32 {
    match len {
        0..=2 => 0,
        3..=5 => 1,
        6..=9 => 2,
        _ => 3,
    }
}

/// Approximate substring match (Sellers' algorithm): the minimal edit
/// distance (insertion, deletion, substitution) between `query` and any
/// substring of `text`. Returns the distance and the text positions whose
/// chars align exactly with query chars (for highlighting), or `None` when
/// even the best substring needs more than `max_errors` edits. `scratch`
/// is the reused DP matrix of `(query.len() + 1) × (text.len() + 1)` cells;
/// every cell is at most `query.len()`, so `u16` is plenty.
fn approx_match(
    query: &[char],
    text: &[char],
    max_errors: u32,
    scratch: &mut Vec<u16>,
) -> Option<(u32, Vec<u32>)> {
    let query_len = query.len();
    let text_len = text.len();
    if query_len == 0 || text_len == 0 || query_len > usize::from(u16::MAX) {
        return None;
    }
    let width = text_len + 1;
    scratch.clear();
    scratch.resize((query_len + 1) * width, 0);
    // Row 0 stays zero: a match may start anywhere in `text`.
    for row in 1..=query_len {
        // Column 0: the first `row` query chars against an empty prefix
        // of `text`, i.e. `row` deletions.
        scratch[row * width] = u16::try_from(row).unwrap_or(u16::MAX);
        for col in 1..=text_len {
            let cost = u16::from(query[row - 1] != text[col - 1]);
            let substitute = scratch[(row - 1) * width + (col - 1)].saturating_add(cost);
            let delete = scratch[(row - 1) * width + col].saturating_add(1);
            let insert = scratch[row * width + (col - 1)].saturating_add(1);
            scratch[row * width + col] = substitute.min(delete).min(insert);
        }
    }
    // Best end position in the last row (leftmost on ties).
    let (mut col, best) = scratch[query_len * width..]
        .iter()
        .copied()
        .enumerate()
        .min_by_key(|&(_, d)| d)?;
    if u32::from(best) > max_errors {
        return None;
    }
    // Trace back, preferring diagonal steps so equal chars get highlighted.
    let mut row = query_len;
    let mut matched: Vec<u32> = Vec::new();
    while row > 0 && col > 0 {
        let here = scratch[row * width + col];
        let cost = u16::from(query[row - 1] != text[col - 1]);
        if here == scratch[(row - 1) * width + (col - 1)].saturating_add(cost) {
            if cost == 0 {
                if let Ok(pos) = u32::try_from(col - 1) {
                    matched.push(pos);
                }
            }
            row -= 1;
            col -= 1;
        } else if here == scratch[(row - 1) * width + col].saturating_add(1) {
            row -= 1;
        } else {
            col -= 1;
        }
    }
    matched.reverse();
    Some((u32::from(best), matched))
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

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

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
        let w = push_truncated_name(&mut s, "abc", &[], 10, RESET);
        assert_eq!(s, "abc");
        assert_eq!(w, 3);
    }

    #[test]
    fn truncated_name_adds_ellipsis() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abcdef", &[], 4, RESET);
        assert_eq!(s, "abc…");
        assert_eq!(w, 4);
    }

    #[test]
    fn truncated_name_wide_chars_respect_cells() {
        // "世界世" = 6 cells; budget 5 reserves 1 for '…', fits 4 cells of name.
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "世界世", &[], 5, RESET);
        assert_eq!(s, "世界…");
        assert_eq!(w, 5);
    }

    #[test]
    fn truncated_name_wide_char_partial_cell() {
        // Budget 4 → content budget 3, next char is 2-wide → only "世" fits.
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "世界世", &[], 4, RESET);
        assert_eq!(s, "世…");
        assert_eq!(w, 3);
    }

    #[test]
    fn truncated_name_single_cell_is_ellipsis() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[], 1, RESET);
        assert_eq!(s, "…");
        assert_eq!(w, 1);
    }

    #[test]
    fn truncated_name_zero_budget_writes_nothing() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[], 0, RESET);
        assert_eq!(s, "");
        assert_eq!(w, 0);
    }

    #[test]
    fn truncated_name_highlights_indices() {
        let mut s = String::new();
        let w = push_truncated_name(&mut s, "abc", &[1], 10, RESET);
        assert_eq!(s, format!("a{MATCH}b{RESET}c")); // escapes are zero-width
        assert_eq!(w, 3);
    }

    #[test]
    fn truncated_name_emits_restore_sequence() {
        // Inside a bell row the restore sequence re-applies the bell bg.
        let mut s = String::new();
        push_truncated_name(&mut s, "abc", &[1], 10, BELL_RESTORE);
        assert_eq!(s, format!("a{MATCH}b{BELL_RESTORE}c"));
    }

    #[test]
    fn name_chars_tolerate_duplicate_indices() {
        // A duplicate must not wedge the index iterator and hide `c`.
        let mut s = String::new();
        push_truncated_name(&mut s, "abc", &[1, 1, 2], 10, RESET);
        assert_eq!(s, format!("a{MATCH}b{RESET}{MATCH}c{RESET}"));
    }

    #[test]
    fn push_pad_loops_past_static_buffer() {
        let mut s = String::new();
        push_pad(&mut s, 0);
        assert_eq!(s, "");
        push_pad(&mut s, 300);
        assert_eq!(s.len(), 300);
        assert!(s.bytes().all(|b| b == b' '));
    }

    #[test]
    fn size_cfg_clamps_to_minimum() {
        let mut cfg = BTreeMap::new();
        cfg.insert("max_cols".to_string(), "0".to_string());
        cfg.insert("max_rows".to_string(), "abc".to_string());
        let size = SizeCfg::from_config(&cfg);
        assert_eq!(size.cols, MIN_BOX_COLS); // clamped
        assert_eq!(size.rows, 20); // unparsable → default
        cfg.insert("max_cols".to_string(), "200".to_string());
        cfg.insert("max_rows".to_string(), "2".to_string());
        let size = SizeCfg::from_config(&cfg);
        assert_eq!(size.cols, 200);
        assert_eq!(size.rows, MIN_BOX_ROWS);
    }

    #[test]
    fn haystack_keeps_one_entry_per_codepoint() {
        // "e" + combining acute + "x": three codepoints, two graphemes.
        let name = "e\u{301}x";
        match haystack_for(name) {
            Utf32String::Unicode(chars) => assert_eq!(chars.len(), 3),
            Utf32String::Ascii(_) => panic!("non-ASCII name must use the Unicode variant"),
        }
        assert!(matches!(haystack_for("plain"), Utf32String::Ascii(_)));
    }

    #[test]
    fn match_kind_rank_orders_fuzzy_before_approx() {
        let mut kinds = vec![
            MatchKind::Approx(2),
            MatchKind::Fuzzy(10),
            MatchKind::Approx(0),
            MatchKind::Fuzzy(500),
        ];
        kinds.sort_by_key(|k| k.rank());
        assert_eq!(
            kinds,
            vec![
                MatchKind::Fuzzy(500),
                MatchKind::Fuzzy(10),
                MatchKind::Approx(0),
                MatchKind::Approx(2),
            ]
        );
    }

    #[test]
    fn max_errors_thresholds() {
        assert_eq!(max_errors(0), 0);
        assert_eq!(max_errors(2), 0);
        assert_eq!(max_errors(3), 1);
        assert_eq!(max_errors(5), 1);
        assert_eq!(max_errors(6), 2);
        assert_eq!(max_errors(9), 2);
        assert_eq!(max_errors(10), 3);
    }

    #[test]
    fn approx_match_tolerates_typos() {
        let mut scratch = Vec::new();
        let name = chars("backend");
        // "bakend": the c is missing → distance 1, every other char
        // highlighted.
        assert_eq!(
            approx_match(&chars("bakend"), &name, 1, &mut scratch),
            Some((1, vec![0, 1, 3, 4, 5, 6]))
        );
        // "bacckend": one c too many → distance 1, the whole name highlighted.
        assert_eq!(
            approx_match(&chars("bacckend"), &name, 1, &mut scratch),
            Some((1, vec![0, 1, 2, 3, 4, 5, 6]))
        );
        // Exact substring → distance 0, every char highlighted.
        assert_eq!(
            approx_match(&chars("end"), &name, 1, &mut scratch),
            Some((0, vec![4, 5, 6]))
        );
        // Beyond the budget → no match.
        assert_eq!(approx_match(&chars("xyz"), &name, 1, &mut scratch), None);
        assert_eq!(
            approx_match(&chars("free"), &chars("frontend"), 1, &mut scratch),
            None
        );
        // Degenerate inputs.
        assert_eq!(approx_match(&[], &name, 1, &mut scratch), None);
        assert_eq!(approx_match(&chars("bak"), &[], 1, &mut scratch), None);
    }

    #[test]
    fn timers_resolve_oldest_eligible_entry() {
        let mut t = Timers::default();
        t.armed(TimerKind::Blink, BLINK_INTERVAL);
        t.armed(TimerKind::Jump { generation: 1 }, JUMP_TIMEOUT);
        assert!(t.blink_in_flight());
        // The 0.4 s jump timer fires first; the older blink entry is not
        // eligible yet, so it must not be consumed.
        assert_eq!(t.resolve(0.401), Some(TimerKind::Jump { generation: 1 }));
        assert!(t.blink_in_flight());
        assert_eq!(t.resolve(0.5), Some(TimerKind::Blink));
        assert!(!t.blink_in_flight());
        assert_eq!(t.resolve(0.5), None);
    }

    #[test]
    fn timers_late_event_never_starves_a_jump() {
        let mut t = Timers::default();
        t.armed(TimerKind::Blink, BLINK_INTERVAL);
        t.armed(TimerKind::Jump { generation: 1 }, JUMP_TIMEOUT);
        // A jump event delayed past 0.5 s is attributed to the older blink
        // entry; the following blink event then resolves to the jump, so
        // both handlers still run exactly once.
        assert_eq!(t.resolve(0.55), Some(TimerKind::Blink));
        assert_eq!(t.resolve(0.5), Some(TimerKind::Jump { generation: 1 }));
        // Below every requested duration (clock skew): fall back to oldest.
        t.armed(TimerKind::Jump { generation: 2 }, JUMP_TIMEOUT);
        assert_eq!(t.resolve(0.1), Some(TimerKind::Jump { generation: 2 }));
    }

    #[test]
    fn timers_queue_is_bounded_and_generations_advance() {
        let mut t = Timers::default();
        for _ in 0..(MAX_PENDING_TIMERS + 3) {
            t.armed(TimerKind::Blink, BLINK_INTERVAL);
        }
        assert_eq!(t.pending.len(), MAX_PENDING_TIMERS);
        assert_eq!(t.new_jump_generation(), 1);
        assert_eq!(t.new_jump_generation(), 2);
        assert_eq!(t.jump_generation, 2);
    }

    #[test]
    fn timers_eviction_keeps_the_blink_entry() {
        let mut t = Timers::default();
        let newest = u32::try_from(MAX_PENDING_TIMERS).unwrap() + 2;
        t.armed(TimerKind::Blink, BLINK_INTERVAL);
        for generation in 1..=newest {
            t.armed(TimerKind::Jump { generation }, JUMP_TIMEOUT);
        }
        assert_eq!(t.pending.len(), MAX_PENDING_TIMERS);
        assert!(t.blink_in_flight());
        // The oldest jump timers were evicted, the newest survive.
        assert_eq!(t.pending.front().map(|(k, _)| *k), Some(TimerKind::Blink));
        assert_eq!(
            t.pending.back().map(|(k, _)| *k),
            Some(TimerKind::Jump { generation: newest })
        );
    }

    #[test]
    fn geometry_reassert_rules() {
        let mut state = State::default(); // target 60×20
        let tab = TabInfo {
            position: 3,
            viewport_columns: 100,
            viewport_rows: 30,
            ..TabInfo::default()
        };
        state.tabs.push(TabEntry {
            haystack: haystack_for(&tab.name),
            chars: Vec::new(),
            info: Rc::new(tab),
        });
        // On target: nothing to do.
        assert!(!state.geometry_needs_reassert(3, 60, 20));
        // Larger in any dimension: always.
        assert!(state.geometry_needs_reassert(3, 61, 20));
        assert!(state.geometry_needs_reassert(3, 60, 21));
        // Smaller, but the target fits the viewport: re-assert.
        assert!(state.geometry_needs_reassert(3, 50, 15));
        // Smaller because the viewport is too small: leave alone.
        state.size.cols = 120;
        assert!(!state.geometry_needs_reassert(3, 50, 15));
        // Unknown tab: no viewport information, leave alone.
        state.size.cols = 60;
        assert!(!state.geometry_needs_reassert(7, 50, 15));
    }

    #[test]
    fn visibility_needs_both_local_signals_and_no_host_veto() {
        let mut vis = Visibility {
            pane_focused: true,
            float_visible: true,
            ..Visibility::default()
        };
        assert!(vis.is_visible()); // no host signal at all
        vis.host_visible = Some(true);
        assert!(vis.is_visible());
        vis.host_visible = Some(false);
        assert!(!vis.is_visible()); // host veto
        vis.host_visible = None;
        vis.float_visible = false;
        assert!(!vis.is_visible()); // floating layer hidden
        vis.float_visible = true;
        vis.pane_focused = false;
        assert!(!vis.is_visible()); // not focused / suppressed
    }

    /// Three scored rows over a list area starting at pane row 5.
    fn state_with_rows() -> State {
        let mut state = State::default();
        for position in 0..3 {
            let tab = TabInfo {
                position,
                ..TabInfo::default()
            };
            state.scored.push(Scored {
                kind: MatchKind::Listed,
                tab: Rc::new(tab),
                indices: Vec::new(),
            });
        }
        state.list_area = Some(ListArea {
            top: 5,
            rows: 10,
            left: 4,
            width: 60,
        });
        state
    }

    #[test]
    fn list_index_at_hit_tests_populated_rows_only() {
        let mut state = state_with_rows();
        assert_eq!(state.list_index_at(5, 10), Some(0)); // first list row
        assert_eq!(state.list_index_at(7, 63), Some(2)); // last populated, right edge
        assert_eq!(state.list_index_at(8, 10), None); // empty list row
        assert_eq!(state.list_index_at(4, 10), None); // header above the list
        assert_eq!(state.list_index_at(5, 3), None); // left of the box
        assert_eq!(state.list_index_at(5, 64), None); // right of the box
        assert_eq!(state.list_index_at(-1, 10), None); // negative line

        // Scrolled list: row offsets shift with `scroll_offset`.
        state.scroll_offset = 2;
        assert_eq!(state.list_index_at(5, 10), Some(2));
        assert_eq!(state.list_index_at(6, 10), None);
        // No render yet: nothing to hit.
        state.list_area = None;
        assert_eq!(state.list_index_at(5, 10), None);
    }

    #[test]
    fn move_selection_by_clamps_at_both_ends() {
        let mut state = state_with_rows();
        assert!(state.move_selection_by(Direction::Down, 2));
        assert_eq!(state.selected, 2);
        assert!(!state.move_selection_by(Direction::Down, 5)); // already last
        assert_eq!(state.selected, 2);
        assert!(state.move_selection_by(Direction::Up, 10));
        assert_eq!(state.selected, 0);
        assert!(!state.move_selection_by(Direction::Up, 1)); // already first
        state.scored.clear();
        assert!(!state.move_selection_by(Direction::Down, 1)); // empty list
    }

    /// Tabs by name; position and `tab_id` follow the slice order and the
    /// first tab is the active one (shown as the header, never listed).
    fn state_with_tabs(names: &[&str]) -> State {
        let mut state = State {
            tabs_received: true,
            ..State::default()
        };
        for (position, name) in names.iter().enumerate() {
            let tab = TabInfo {
                position,
                tab_id: position,
                name: (*name).to_string(),
                active: position == 0,
                ..TabInfo::default()
            };
            state.tabs.push(TabEntry {
                haystack: haystack_for(name),
                chars: name.chars().collect(),
                info: Rc::new(tab),
            });
        }
        state
    }

    fn listed_names(state: &State) -> Vec<&str> {
        state.scored.iter().map(|s| s.tab.name.as_str()).collect()
    }

    #[test]
    fn search_ranks_fuzzy_matches_above_typo_matches() {
        let mut state = state_with_tabs(&["active", "backend", "frontend", "notes"]);

        // Typo nucleo cannot bridge (no second k in the name): the approximate
        // pass finds it, every char but the substituted one highlighted.
        state.query = "bakkend".to_string();
        state.refresh_scores();
        assert_eq!(listed_names(&state), vec!["backend"]);
        assert_eq!(state.scored[0].kind, MatchKind::Approx(1));
        assert_eq!(state.scored[0].indices, vec![0, 1, 3, 4, 5, 6]);

        state.query = "back".to_string();
        state.refresh_scores();
        assert_eq!(listed_names(&state), vec!["backend"]);
        assert!(matches!(state.scored[0].kind, MatchKind::Fuzzy(_)));
        assert_eq!(state.scored[0].indices, vec![0, 1, 2, 3]);

        state.query = "bx".to_string(); // two chars: nucleo only, no typo pass
        state.refresh_scores();
        assert!(state.scored.is_empty());

        state.query = "!back".to_string(); // nucleo negation, no typo fallback
        state.refresh_scores();
        assert_eq!(listed_names(&state), vec!["frontend", "notes"]);

        state.query = "zzz".to_string();
        state.refresh_scores();
        assert!(state.scored.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn empty_query_lists_by_frequency_then_position() {
        let mut state = state_with_tabs(&["active", "alpha", "beta", "gamma"]);
        state.access_counts.insert(3, 5); // gamma
        state.access_counts.insert(2, 1); // beta
        state.refresh_scores();
        assert_eq!(listed_names(&state), vec!["gamma", "beta", "alpha"]);
        assert!(state.scored.iter().all(|s| s.kind == MatchKind::Listed));
    }

    #[test]
    fn rendered_rows_are_exactly_one_pane_wide() {
        let mut state = state_with_tabs(&[
            "aktiv",
            "backend",
            "日本語のタブ",
            "🚀 deploy",
            "a-very-long-tab-name-that-does-not-fit-into-the-box-at-all-really",
        ]);
        let mut bell = (*state.tabs[1].info).clone();
        bell.has_bell_notification = true;
        state.tabs[1].info = Rc::new(bell);
        state.timers.bell_blink_on = true;

        // Pane larger than the 60×20 box: padding on every side, wide
        // chars, a selected bell row, a truncated name.
        state.refresh_scores();
        let mut out = String::new();
        state.render_box(&mut out, BoxPlacement::centered(24, 80, &state.size));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 20);
        for line in &lines {
            assert_eq!(visible_len(line), 80, "{line:?}");
        }
        assert!(out.contains('…'));
        // vpad_top 2 + top border, prompt, separator, header, separator.
        assert_eq!(
            state.list_area,
            Some(ListArea {
                top: 7,
                rows: 14,
                left: 10,
                width: 60
            })
        );

        // Bell row with match highlights keeps every row pane-wide too.
        state.query = "a".to_string();
        state.refresh_scores();
        let mut out = String::new();
        state.render_box(&mut out, BoxPlacement::centered(24, 80, &state.size));
        assert!(out.contains(BELL_RESTORE));
        assert!(out.lines().all(|line| visible_len(line) == 80));

        // Pane smaller than the box: the box shrinks, rows stay pane-wide.
        let mut out = String::new();
        state.render_box(&mut out, BoxPlacement::centered(10, 40, &state.size));
        assert_eq!(out.lines().count(), 10);
        assert!(out.lines().all(|line| visible_len(line) == 40));

        // Too small for the frame: blank rows, no list area.
        let mut out = String::new();
        state.render_box(&mut out, BoxPlacement::centered(3, 80, &state.size));
        assert_eq!(out.lines().count(), 3);
        assert!(state.list_area.is_none());

        // No matches, and "list not yet known" before the first TabUpdate.
        state.query = "zzz".to_string();
        state.refresh_scores();
        let mut out = String::new();
        state.render_box(&mut out, BoxPlacement::centered(20, 60, &state.size));
        assert!(out.contains("no matches"));
        state.tabs_received = false;
        let mut out = String::new();
        state.render_box(&mut out, BoxPlacement::centered(20, 60, &state.size));
        assert!(!out.contains("no "));
    }
}
