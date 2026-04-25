use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::rc::Rc;
use zellij_tile::prelude::*;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32String};

// SGR-Sequenzen — nutzen die 16-ANSI-Palette (Zellij-/Terminal-Theme).
const RESET: &str = "\u{1b}[0m";
const DIM: &str = "\u{1b}[2m";
const BOLD: &str = "\u{1b}[1m";
const REVERSE: &str = "\u{1b}[7m";
const PROMPT: &str = "\u{1b}[36;1m"; // bold cyan
const MATCH: &str = "\u{1b}[33;1m"; // bold yellow
const FRAME: &str = "\u{1b}[2m";
const ACTIVE: &str = "\u{1b}[1;4;36m"; // bold underline cyan

// Statisches Padding, um `" ".repeat(n)`-Allocations in Hot-Paths zu vermeiden.
const PAD_SPACES: &str = "                                                                                                                                ";

/// Slice für kleine, bekannte n (z.B. format!-Inserts). Für beliebig große n
/// (Terminal-Breiten > 128) benutze `push_pad` — das loopt korrekt weiter.
fn pad(n: usize) -> &'static str {
    &PAD_SPACES[..n.min(PAD_SPACES.len())]
}

/// Schreibt exakt `n` Leerzeichen in `out`, auch wenn `n > PAD_SPACES.len()`.
/// Wichtig für ultrawide Terminals, damit Lead/Trail/Leerzeilen nicht zu kurz
/// sind (würde zu transparenten Cells führen).
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

// Harte Obergrenze für die Query — verhindert Layout-Overflow bei Paste von
// riesigen Strings und bewahrt die Prompt-Row-Integrität.
const MAX_QUERY_CHARS: usize = 128;

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

#[derive(Clone, Copy)]
struct DimCfg {
    min_abs: usize,
    max_abs: usize,
    max_percent: usize,
}

impl DimCfg {
    fn target(&self, avail: usize) -> usize {
        let rel_cap = avail.saturating_mul(self.max_percent) / 100;
        let upper = self.max_abs.min(rel_cap);
        let lower = self.min_abs.min(avail);
        upper.max(lower).min(avail)
    }
}

struct SizeCfg {
    cols: DimCfg,
    rows: DimCfg,
}

impl Default for SizeCfg {
    fn default() -> Self {
        Self {
            cols: DimCfg {
                min_abs: 40,
                max_abs: 90,
                max_percent: 60,
            },
            rows: DimCfg {
                min_abs: 10,
                max_abs: 22,
                max_percent: 50,
            },
        }
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
            cols: DimCfg {
                min_abs: parse(cfg, "min_cols", d.cols.min_abs),
                max_abs: parse(cfg, "max_cols", d.cols.max_abs),
                max_percent: parse(cfg, "max_cols_percent", d.cols.max_percent),
            },
            rows: DimCfg {
                min_abs: parse(cfg, "min_rows", d.rows.min_abs),
                max_abs: parse(cfg, "max_rows", d.rows.max_abs),
                max_percent: parse(cfg, "max_rows_percent", d.rows.max_percent),
            },
        }
    }
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
    ready: bool,
    // Zugriffs-Frequenz via Picker (Enter / Instant-Jump). Häufigster Tab zuerst.
    // Aktiver Tab wird vor dem Sortieren rausgefiltert und als Header gezeigt.
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
            EventType::TabUpdate,
            EventType::Key,
            EventType::PermissionRequestResult,
        ]);
        self.size = SizeCfg::from_config(&configuration);
        // Unsichtbar, bis Permission da ist und borderless gesetzt wurde (kein Flicker).
        hide_self();
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                let ids = get_plugin_ids();
                set_pane_borderless(PaneId::Plugin(ids.plugin_id), true);
                self.ready = true;
                show_self(true);
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                // Ohne ChangeApplicationState können wir weder Borderless setzen
                // noch Tabs wechseln. Plugin schließen statt stumm hängen zu bleiben.
                close_self();
                false
            }
            Event::TabUpdate(new_tabs) => {
                // Dedup: Zellij feuert TabUpdate häufig ohne Änderung.
                if self.tabs.len() == new_tabs.len()
                    && new_tabs.iter().zip(&self.tabs).all(|(n, e)| *n == *e.info)
                {
                    return false;
                }
                // Utf32String für Fuzzy-Haystacks jetzt einmal pro TabUpdate cachen
                // (vorher: pro Keystroke × Tab).
                self.tabs = new_tabs
                    .into_iter()
                    .map(|t| TabEntry {
                        haystack: Utf32String::from(t.name.as_str()),
                        info: Rc::new(t),
                    })
                    .collect();
                self.refresh_scores();
                true
            }
            Event::Key(key) => {
                // Keys vor Permission/show ignorieren — sonst füllt sich die Query,
                // während die Pane noch unsichtbar/initialisiert ist, und erscheint
                // beim ersten Render unerwartet gefüllt.
                if !self.ready {
                    return false;
                }
                self.handle_key(&key)
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        // Ein String-Buffer, ein print! → 1 Syscall statt ~30.
        let mut out = String::with_capacity(rows * (cols + 8));

        if !self.ready {
            for _ in 0..rows {
                append_padded(&mut out, cols, "");
            }
            print!("{out}");
            return;
        }

        let tcols = self.size.cols.target(cols);
        let trows = self.size.rows.target(rows);
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

fn append_wrap(out: &mut String, content: &str, content_width: usize, selected: bool) {
    let vis = visible_len(content);
    let gap = content_width.saturating_sub(vis);
    out.push_str(FRAME);
    out.push('│');
    out.push_str(RESET);
    out.push(' ');
    if selected {
        out.push_str(BOLD);
        out.push_str(REVERSE);
    }
    out.push(' ');
    out.push_str(content);
    out.push_str(pad(gap));
    out.push(' ');
    if selected {
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
    // Schreibt eine horizontale Border-Zeile (lead + ╭─╮ / ├─┤ / ╰─╯ + trail + \n).
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

    // Schreibt eine umrahmte Content-Zeile (lead + │ + content + │ + trail + \n).
    fn write_wrapped(
        out: &mut String,
        hpad: usize,
        trail_n: usize,
        content: &str,
        content_width: usize,
        selected: bool,
    ) {
        push_pad(out, hpad);
        append_wrap(out, content, content_width, selected);
        push_pad(out, trail_n);
        out.push('\n');
    }

    fn build_prompt(&self, content: &mut String, content_width: usize) {
        let count_vis = digit_count(self.scored.len()) + 1 + digit_count(self.tabs.len());
        // "❯ " (2) + cursor " " (1) = 3 fixe Prompt-Chars links, count_vis rechts.
        let query_budget = content_width.saturating_sub(3 + count_vis);
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
        let prompt_vis = 3 + query_display.chars().count();
        let mid_pad = content_width.saturating_sub(prompt_vis + count_vis);
        push_pad(content, mid_pad);
        content.push_str(DIM);
        let _ = write!(content, "{}/{}", self.scored.len(), self.tabs.len());
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
                Self::write_wrapped(out, hpad, trail_n, content, content_width, false);
            }
            return;
        }

        if self.selected >= self.scroll_offset + list_rows {
            self.scroll_offset = self.selected + 1 - list_rows;
        } else if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }

        for row in 0..list_rows {
            let idx = self.scroll_offset + row;
            let is_sel = idx == self.selected;
            content.clear();
            if let Some(s) = self.scored.get(idx) {
                render_row(content, s, is_sel, content_width);
            }
            Self::write_wrapped(out, hpad, trail_n, content, content_width, is_sel);
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
        // Mindestbreite: Frame(6) + Mindestcontent. Unter 20 Cols blank.
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
        Self::write_wrapped(out, hpad, trail_n, &content, content_width, false);

        Self::write_border(out, hpad, trail_n, '├', '┤', cols);

        let mut list_rows = rows.saturating_sub(4);
        if let Some(t) = &active_tab {
            if list_rows >= 2 {
                content.clear();
                render_active_row(&mut content, t, content_width);
                Self::write_wrapped(out, hpad, trail_n, &content, content_width, false);
                Self::write_border(out, hpad, trail_n, '├', '┤', cols);
                list_rows -= 2;
            }
        }

        self.render_list(out, &mut content, list_rows, hpad, trail_n, content_width);

        Self::write_border(out, hpad, trail_n, '╰', '╯', cols);
    }

    fn hide(&mut self) {
        // Query leer für den nächsten Open-Zyklus; Plugin-Instanz + Counts überleben.
        self.query.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.refresh_scores();
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
        self.selected = 0;
        self.refresh_scores();
    }

    // Springt auf Tab-Position `pos` (1-based). Returns true, wenn die Position
    // existiert und der Picker geschlossen wurde.
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
                    // pos ist usize; + 1 und Umwandlung in u32 beide fehlbar.
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
                if !has_ctrl
                    && !has_alt
                    && self.query.is_empty()
                    && c.is_ascii_digit()
                    && c != '0' =>
            {
                // c ist garantiert ASCII-Ziffer '1'..='9' → to_digit(10) ist infallible.
                let pos = c.to_digit(10).unwrap_or(0);
                if self.try_instant_jump(pos) {
                    return false;
                }
                self.query.push(c);
                self.query_changed();
                true
            }
            BareKey::Char(c) if !has_ctrl && !has_alt => {
                // Control-Chars (z.B. via Paste) ignorieren — verhindert SGR-Injection.
                // Hard-Cap: verhindert dass lange Pastes das Prompt-Layout sprengen.
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
            // Frequency-Sort. Aktiver Tab ausgefiltert (Header).
            let counts = &self.access_counts;
            self.scored = self
                .tabs
                .iter()
                .filter(|e| !e.info.active)
                .map(|e| Scored {
                    score: 0,
                    tab: Rc::clone(&e.info),
                    indices: Vec::new(),
                })
                .collect();
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
    let mut pane_str = String::with_capacity(6);
    let _ = write!(pane_str, "[{panes}]");
    let pane_len = visible_len(&pane_str);
    let bell = if tab.has_bell_notification { '!' } else { ' ' };
    // Layout: name + gap + " " + bell + " " + pane_str
    let name_area = cols.saturating_sub(3 + pane_len);

    // Defensive: bei extrem schmalen Panes nur truncated Name zeigen, kein Overflow.
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
    out.push(' ');
    out.push(bell);
    out.push(' ');
    out.push_str(&pane_str);
    out.push_str(RESET);
}

fn render_row(out: &mut String, s: &Scored, selected: bool, cols: usize) {
    let tab = &*s.tab;
    let mut num_prefix = String::with_capacity(6);
    let _ = write!(num_prefix, "{} - ", tab.position + 1);
    let num_prefix_len = visible_len(&num_prefix);

    let panes = tab.selectable_tiled_panes_count + tab.selectable_floating_panes_count;
    let mut pane_str = String::with_capacity(6);
    let _ = write!(pane_str, "[{panes}]");
    let pane_len = visible_len(&pane_str);
    let bell_ch = if tab.has_bell_notification { '!' } else { ' ' };
    // Layout: num_prefix + name + gap + " " + bell + " " + pane_str
    let name_area = cols.saturating_sub(num_prefix_len + 3 + pane_len);

    // Defensive: bei extrem schmalen Panes (z.B. 1000+ Tabs) fallen num_prefix und
    // pane_str zu groß aus für das content_width. Nur Name rendern, kein Overflow.
    if name_area == 0 {
        let written = push_truncated_name(out, &tab.name, &[], cols);
        push_pad(out, cols.saturating_sub(written));
        return;
    }

    if selected {
        // Plain text — wrap_row legt das Selected-Highlight drum.
        out.push_str(&num_prefix);
        let name_written = push_truncated_name(out, &tab.name, &[], name_area);
        out.push_str(pad(name_area.saturating_sub(name_written)));
        out.push(' ');
        out.push(bell_ch);
        out.push(' ');
        out.push_str(&pane_str);
    } else {
        out.push_str(DIM);
        out.push_str(&num_prefix);
        out.push_str(RESET);
        let name_written = push_truncated_name(out, &tab.name, &s.indices, name_area);
        out.push_str(pad(name_area.saturating_sub(name_written)));
        out.push(' ');
        if tab.has_bell_notification {
            out.push_str(MATCH);
            out.push(bell_ch);
            out.push_str(RESET);
        } else {
            out.push(bell_ch);
        }
        out.push(' ');
        out.push_str(DIM);
        out.push_str(&pane_str);
        out.push_str(RESET);
    }
}

/// Schreibt den (ggf. abgeschnittenen) Tab-Namen direkt in `out`,
/// mit Match-Highlight an allen `indices` (müssen sortiert aufsteigend sein).
/// Gibt die Anzahl der sichtbaren Zeichen zurück, die geschrieben wurden.
fn push_truncated_name(out: &mut String, name: &str, indices: &[u32], max_chars: usize) -> usize {
    if max_chars == 0 {
        return 0;
    }
    let total = name.chars().count();
    let (take, tail) = if total <= max_chars {
        (total, false)
    } else if max_chars <= 1 {
        out.push('…');
        return 1;
    } else {
        (max_chars - 1, true)
    };

    let mut idx_iter = indices.iter().peekable();
    for (i, c) in name.chars().take(take).enumerate() {
        // Name-Länge <= max_chars <= cols (realistisch klein). Falls ein Tab-Name
        // dennoch > u32::MAX Zeichen hätte, brechen wir still ab — besser als
        // silent-truncation durch `as u32`.
        let Ok(i_u32) = u32::try_from(i) else { break };
        // Control-Chars / ANSI-Escapes aus Tab-Namen filtern, damit sie unseren
        // SGR-State nicht durchbrechen können.
        let c = sanitize_char(c);
        if idx_iter.peek().is_some_and(|&&v| v == i_u32) {
            idx_iter.next();
            out.push_str(MATCH);
            out.push(c);
            out.push_str(RESET);
        } else {
            out.push(c);
        }
    }
    if tail {
        out.push('…');
        take + 1
    } else {
        take
    }
}

/// Ersetzt Control-Chars (inkl. ESC) durch '?' — verhindert SGR-/Cursor-Injection
/// durch Tab-Namen oder Query-Input.
fn sanitize_char(c: char) -> char {
    if c.is_control() {
        '?'
    } else {
        c
    }
}

/// Anzahl der Ziffern einer Zahl — für Pre-Compute von Count-Display-Breiten.
fn digit_count(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        n.ilog10() as usize + 1
    }
}

/// Liefert das Ende der Query, wenn sie zu lang für das Display-Budget ist.
/// Beim Tippen ist der Cursor am Ende — den will der User sehen.
fn query_tail(query: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    let total = query.chars().count();
    if total <= max_chars {
        return query;
    }
    let skip = total - max_chars;
    let byte_idx = query
        .char_indices()
        .nth(skip)
        .map_or(query.len(), |(i, _)| i);
    &query[byte_idx..]
}

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
        len += 1;
    }
    len
}
