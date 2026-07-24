//! The add-item Popover: layout, hit-testing, and single-line text editing.
//! Pure logic — no GPU, no winit, no Win32 — so all of it is unit-testable.
//! Coordinates are relative to the Menu center (which is the window center),
//! in px, so they survive the window growing while Pinned.

use std::path::Path;

/// Panel dimensions, px.
pub const PANEL_W: f32 = 320.0;
pub const PANEL_H: f32 = 224.0;
/// Gap between the Scrim's edge and the panel.
pub const GAP: f32 = 12.0;

/// Center + half-extents rectangle, Menu-center-relative px.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub center: [f32; 2],
    pub half: [f32; 2],
}

impl Rect {
    fn at(left: f32, top: f32, w: f32, h: f32) -> Rect {
        Rect {
            center: [left + w / 2.0, top + h / 2.0],
            half: [w / 2.0, h / 2.0],
        }
    }

    pub fn contains(&self, p: (f32, f32)) -> bool {
        (p.0 - self.center[0]).abs() <= self.half[0] && (p.1 - self.center[1]).abs() <= self.half[1]
    }

    pub fn left(&self) -> f32 {
        self.center[0] - self.half[0]
    }

    pub fn top(&self) -> f32 {
        self.center[1] - self.half[1]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Element {
    NameField,
    TargetField,
    Browse,
    IconBtn,
    Commit,
    Cancel,
}

/// Side effects the caller (main) must perform; everything else is internal state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Commit,
    Discard,
    Browse,
    BrowseIcon,
}

/// Keys the editor understands, decoupled from winit for testability.
/// Plain text goes through `PopoverState::insert` directly.
#[derive(Clone, Debug)]
pub enum Key {
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Tab,
    Enter,
    Escape,
}

#[derive(Clone, Copy)]
pub struct Layout {
    pub panel: Rect,
    pub name_field: Rect,
    pub target_field: Rect,
    pub browse_btn: Rect,
    pub icon_preview: Rect,
    pub icon_btn: Rect,
    pub commit_btn: Rect,
    pub cancel_btn: Rect,
}

impl Layout {
    pub fn new(panel_center: [f32; 2]) -> Layout {
        let (x, y) = (
            panel_center[0] - PANEL_W / 2.0,
            panel_center[1] - PANEL_H / 2.0,
        );
        let pad = 16.0;
        let field_h = 30.0;
        Layout {
            panel: Rect {
                center: panel_center,
                half: [PANEL_W / 2.0, PANEL_H / 2.0],
            },
            name_field: Rect::at(x + pad, y + 26.0, PANEL_W - 2.0 * pad, field_h),
            target_field: Rect::at(x + pad, y + 76.0, PANEL_W - 2.0 * pad - 38.0, field_h),
            browse_btn: Rect::at(x + PANEL_W - pad - 30.0, y + 76.0, 30.0, field_h),
            icon_preview: Rect::at(x + pad, y + 118.0, 48.0, 48.0),
            icon_btn: Rect::at(x + pad + 60.0, y + 127.0, 84.0, field_h),
            cancel_btn: Rect::at(x + PANEL_W - pad - 156.0, y + PANEL_H - 46.0, 72.0, field_h),
            commit_btn: Rect::at(x + PANEL_W - pad - 76.0, y + PANEL_H - 46.0, 76.0, field_h),
        }
    }

    pub fn hit(&self, p: (f32, f32)) -> Option<Element> {
        [
            (Element::NameField, self.name_field),
            (Element::TargetField, self.target_field),
            (Element::Browse, self.browse_btn),
            (Element::IconBtn, self.icon_btn),
            (Element::Commit, self.commit_btn),
            (Element::Cancel, self.cancel_btn),
        ]
        .into_iter()
        .find(|(_, r)| r.contains(p))
        .map(|(e, _)| e)
    }
}

/// Single-line editable text with a caret (byte index, always on a char
/// boundary). ponytail: no selection — drag/shift machinery buys nothing in
/// two short fields.
#[derive(Default, Clone)]
pub struct Field {
    pub text: String,
    pub caret: usize,
}

impl Field {
    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.caret]
            .chars()
            .next_back()
            .map(|c| self.caret - c.len_utf8())
    }

    pub fn backspace(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.text.remove(p);
            self.caret = p;
        }
    }

    pub fn delete(&mut self) {
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }

    pub fn left(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.caret = p;
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.caret..].chars().next() {
            self.caret += c.len_utf8();
        }
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.caret = s.len();
    }
}

pub struct PopoverState {
    pub layout: Layout,
    /// The Dodaj Tile's center, Menu-center-relative — the expand animation origin.
    pub origin: [f32; 2],
    pub name: Field,
    pub target: Field,
    /// NameField or TargetField.
    pub focus: Element,
    /// The user typed in the name field directly; browse auto-fill backs off.
    pub name_touched: bool,
    /// Explicit PNG chosen via the icon button; overrides auto-extraction.
    pub icon_override: Option<String>,
    pub hover: Option<Element>,
    /// Bumped on every edit; gfx reshapes its text buffers when it changes.
    pub generation: u64,
}

impl PopoverState {
    pub fn new(layout: Layout, origin: [f32; 2]) -> PopoverState {
        PopoverState {
            layout,
            origin,
            name: Field::default(),
            target: Field::default(),
            focus: Element::NameField,
            name_touched: false,
            icon_override: None,
            hover: None,
            generation: 0,
        }
    }

    pub fn valid(&self) -> bool {
        !self.target.text.trim().is_empty()
    }

    fn focused_field(&mut self) -> &mut Field {
        if self.focus == Element::TargetField {
            &mut self.target
        } else {
            &mut self.name
        }
    }

    /// Insert text into the focused field (typing, paste, IME commit).
    pub fn insert(&mut self, s: &str) {
        if self.focus == Element::NameField {
            self.name_touched = true;
        }
        self.focused_field().insert(s);
        self.generation += 1;
    }

    pub fn on_click(&mut self, el: Element) -> Option<Action> {
        match el {
            // Focus/caret only — no text changed, so no reshape is needed.
            Element::NameField | Element::TargetField => {
                self.focus = el;
                self.focused_field().end_caret();
                None
            }
            Element::Browse => Some(Action::Browse),
            Element::IconBtn => Some(Action::BrowseIcon),
            Element::Commit => self.valid().then_some(Action::Commit),
            Element::Cancel => Some(Action::Discard),
        }
    }

    pub fn on_key(&mut self, key: Key) -> Option<Action> {
        // Only bump `generation` for edits that actually change field text —
        // pure focus/caret moves (Tab, arrows, Home/End) reuse the already-
        // shaped buffer, so a reshape in gfx would be wasted work.
        match key {
            Key::Escape => return Some(Action::Discard),
            Key::Enter => return self.valid().then_some(Action::Commit),
            Key::Tab => {
                self.focus = if self.focus == Element::NameField {
                    Element::TargetField
                } else {
                    Element::NameField
                };
            }
            Key::Backspace => {
                if self.focus == Element::NameField {
                    self.name_touched = true;
                }
                self.focused_field().backspace();
                self.generation += 1;
            }
            Key::Delete => {
                if self.focus == Element::NameField {
                    self.name_touched = true;
                }
                self.focused_field().delete();
                self.generation += 1;
            }
            Key::Left => self.focused_field().left(),
            Key::Right => self.focused_field().right(),
            Key::Home => self.focused_field().caret = 0,
            Key::End => self.focused_field().end_caret(),
        }
        None
    }

    /// Browse picked a file: fill target, auto-fill an untouched empty name.
    pub fn apply_picked_file(&mut self, path: &str) {
        self.target.set(path);
        self.focus = Element::TargetField;
        if !self.name_touched && self.name.text.is_empty() {
            let stem = Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            self.name.set(&stem);
            // Deliberately NOT name_touched: the user still hasn't typed a name.
        }
        self.generation += 1;
    }

    /// The Item name to save: typed name, else derived from the target.
    pub fn final_name(&self) -> String {
        let typed = self.name.text.trim();
        if !typed.is_empty() {
            return typed.to_string();
        }
        fallback_name(self.target.text.trim())
    }
}

impl Field {
    fn end_caret(&mut self) {
        self.caret = self.text.len();
    }
}

/// Place a panel's center on one axis so a panel of half-extent `half` stays
/// within the work-area span `[lo, hi]`. When the span is narrower than the
/// panel, center the panel instead — `f32::clamp` PANICS on an inverted range,
/// which a tiny monitor or odd multi-monitor layout would otherwise trigger.
pub fn clamp_panel_axis(desired: f32, half: f32, lo: f32, hi: f32) -> f32 {
    let (min, max) = (lo + half, hi - half);
    if min <= max {
        desired.clamp(min, max)
    } else {
        (lo + hi) / 2.0
    }
}

/// Name for an Item the user didn't name: URL host, else file stem, else the
/// raw target. Always lowercased, like the rest of the Menu's text.
pub fn fallback_name(target: &str) -> String {
    if let Some((_, rest)) = target.split_once("://") {
        let host = rest.split('/').next().unwrap_or(rest);
        if !host.is_empty() {
            return host.to_lowercase();
        }
    }
    Path::new(target)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| target.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PopoverState {
        PopoverState::new(Layout::new([0.0, 0.0]), [0.0, 0.0])
    }

    #[test]
    fn field_edits_respect_utf8_boundaries() {
        let mut f = Field::default();
        f.insert("żółw");
        assert_eq!(f.caret, "żółw".len());
        f.backspace(); // remove 'w'
        assert_eq!(f.text, "żół");
        f.left(); // caret before 'ł'
        f.backspace(); // remove 'ó'
        assert_eq!(f.text, "żł");
        f.right();
        assert_eq!(f.caret, f.text.len());
        f.caret = 0;
        f.delete();
        assert_eq!(f.text, "ł");
    }

    #[test]
    fn tab_toggles_focus() {
        let mut s = state();
        assert_eq!(s.focus, Element::NameField);
        s.on_key(Key::Tab);
        assert_eq!(s.focus, Element::TargetField);
        s.on_key(Key::Tab);
        assert_eq!(s.focus, Element::NameField);
    }

    #[test]
    fn commit_requires_a_target() {
        let mut s = state();
        assert_eq!(s.on_key(Key::Enter), None);
        s.on_key(Key::Tab);
        s.insert("wt.exe");
        assert_eq!(s.on_key(Key::Enter), Some(Action::Commit));
    }

    #[test]
    fn browse_autofills_only_an_untouched_empty_name() {
        let mut s = state();
        s.apply_picked_file(r"C:\Apps\Obsidian.exe");
        assert_eq!(s.name.text, "obsidian");
        assert_eq!(s.target.text, r"C:\Apps\Obsidian.exe");

        // Typed name survives a later browse.
        let mut s = state();
        s.insert("moja");
        s.apply_picked_file(r"C:\Apps\Obsidian.exe");
        assert_eq!(s.name.text, "moja");
    }

    #[test]
    fn fallback_name_prefers_url_host_then_stem() {
        assert_eq!(fallback_name("https://google.com/maps"), "google.com");
        assert_eq!(fallback_name(r"C:\Tools\Notepad.EXE"), "notepad");
        assert_eq!(fallback_name("wt.exe"), "wt");
    }

    #[test]
    fn hit_finds_elements_and_misses_the_panel_body() {
        let l = Layout::new([0.0, 0.0]);
        let c = l.commit_btn.center;
        assert_eq!(l.hit((c[0], c[1])), Some(Element::Commit));
        // Panel's top-left corner area: inside the panel, on no element.
        assert_eq!(l.hit((l.panel.left() + 2.0, l.panel.top() + 2.0)), None);
        assert!(
            l.panel
                .contains((l.panel.left() + 2.0, l.panel.top() + 2.0))
        );
    }

    #[test]
    fn escape_discards_and_click_cancel_discards() {
        let mut s = state();
        assert_eq!(s.on_key(Key::Escape), Some(Action::Discard));
        assert_eq!(s.on_click(Element::Cancel), Some(Action::Discard));
    }

    /// The caret is a byte index into `text`; any op leaving it off a char
    /// boundary or past the end makes the NEXT insert/remove panic. Hammer a
    /// mixed sequence with multibyte text and assert the invariant every step.
    #[test]
    fn caret_never_leaves_a_valid_boundary() {
        fn ok(f: &Field) {
            assert!(f.caret <= f.text.len(), "caret {} > len {}", f.caret, f.text.len());
            assert!(f.text.is_char_boundary(f.caret), "caret {} mid-char in {:?}", f.caret, f.text);
        }
        let mut f = Field::default();
        // Backspace/delete/left/right on an empty field must not panic.
        f.backspace();
        f.delete();
        f.left();
        f.right();
        ok(&f);
        for chunk in ["ą", "b", "łóź", "😀", "x"] {
            f.insert(chunk);
            ok(&f);
        }
        // Walk left past the start, deleting as we go; then right past the end.
        for _ in 0..20 {
            f.left();
            ok(&f);
            f.delete();
            ok(&f);
        }
        for _ in 0..20 {
            f.right();
            ok(&f);
            f.backspace();
            ok(&f);
        }
        assert_eq!(f.text, "");
    }

    #[test]
    fn boundary_ops_are_noops_not_panics() {
        let mut f = Field::default();
        f.set("ab");
        f.caret = f.text.len();
        f.delete(); // at end: nothing to delete
        f.right(); // at end: caret stays
        assert_eq!((f.text.as_str(), f.caret), ("ab", 2));
        f.caret = 0;
        f.backspace(); // at start: nothing
        f.left(); // at start: caret stays
        assert_eq!((f.text.as_str(), f.caret), ("ab", 0));
    }

    #[test]
    fn typing_in_target_does_not_block_name_autofill() {
        // Only NAME edits count as "touched"; a target the user typed still
        // leaves the name free for browse to auto-fill.
        let mut s = state();
        s.on_key(Key::Tab); // focus target
        s.insert("C:/x");
        assert!(!s.name_touched);
        s.apply_picked_file(r"C:\Apps\Obsidian.exe");
        assert_eq!(s.name.text, "obsidian");
    }

    #[test]
    fn name_edit_then_clear_still_counts_as_touched() {
        let mut s = state();
        s.insert("m");
        s.on_key(Key::Backspace); // name now empty but was touched
        assert!(s.name.text.is_empty() && s.name_touched);
        s.apply_picked_file(r"C:\Apps\Obsidian.exe");
        assert_eq!(s.name.text, ""); // not overwritten
    }

    #[test]
    fn apply_picked_file_tolerates_an_odd_path() {
        let mut s = state();
        s.apply_picked_file(r"C:\Apps\"); // trailing separator: no panic
        assert_eq!(s.target.text, r"C:\Apps\");
        // Whatever name got derived, target's caret must stay on a boundary.
        assert!(s.target.text.is_char_boundary(s.target.caret));
        // And an empty path must not panic either.
        s.apply_picked_file("");
        assert_eq!(s.target.text, "");
    }

    #[test]
    fn whitespace_only_target_is_invalid() {
        let mut s = state();
        s.on_key(Key::Tab);
        s.insert("   ");
        assert!(!s.valid());
        assert_eq!(s.on_key(Key::Enter), None);
        assert_eq!(s.on_click(Element::Commit), None);
    }

    #[test]
    fn fallback_name_edge_cases() {
        assert_eq!(fallback_name(""), ""); // empty: file_stem is None, degrades to ""
        assert_eq!(fallback_name("https://GitHub.com/"), "github.com"); // trailing slash + case
        assert_eq!(fallback_name("ftp://host"), "host");
    }

    #[test]
    fn every_layout_element_sits_inside_the_panel() {
        let l = Layout::new([40.0, -30.0]); // non-origin: catch center-relative bugs
        let p = l.panel;
        for r in [
            l.name_field,
            l.target_field,
            l.browse_btn,
            l.icon_preview,
            l.icon_btn,
            l.commit_btn,
            l.cancel_btn,
        ] {
            assert!(r.center[0] - r.half[0] >= p.center[0] - p.half[0], "off left");
            assert!(r.center[0] + r.half[0] <= p.center[0] + p.half[0], "off right");
            assert!(r.center[1] - r.half[1] >= p.center[1] - p.half[1], "off top");
            assert!(r.center[1] + r.half[1] <= p.center[1] + p.half[1], "off bottom");
        }
    }

    #[test]
    fn adjacent_controls_do_not_overlap() {
        let l = Layout::new([0.0, 0.0]);
        let overlaps = |a: Rect, b: Rect| {
            (a.center[0] - b.center[0]).abs() < a.half[0] + b.half[0]
                && (a.center[1] - b.center[1]).abs() < a.half[1] + b.half[1]
        };
        // Pairs that share a row and would swallow each other's clicks if they overlapped.
        assert!(!overlaps(l.commit_btn, l.cancel_btn));
        assert!(!overlaps(l.target_field, l.browse_btn));
        assert!(!overlaps(l.icon_preview, l.icon_btn));
    }

    #[test]
    fn clamp_panel_axis_centers_when_the_area_is_too_small() {
        // Roomy span: normal clamp keeps the panel off the edges.
        assert_eq!(clamp_panel_axis(1000.0, 50.0, 0.0, 400.0), 350.0);
        assert_eq!(clamp_panel_axis(-1000.0, 50.0, 0.0, 400.0), 50.0);
        assert_eq!(clamp_panel_axis(200.0, 50.0, 0.0, 400.0), 200.0);
        // Span narrower than the panel: center it (no inverted-range panic).
        assert_eq!(clamp_panel_axis(9999.0, 200.0, 0.0, 100.0), 50.0);
    }
}
