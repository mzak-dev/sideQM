#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod config;
mod dialog;
mod geometry;
mod gfx;
mod hook;
mod icon_service;
mod icons;
mod launch;
mod logging;
mod media;
mod popover;
mod present;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GetWindowLongPtrW, SetForegroundWindow, SetWindowLongPtrW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::w;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{ModifiersState, NamedKey};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId, WindowLevel};

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::config::Config;
use crate::dialog::PickPurpose;
use crate::geometry::{MenuGeometry, TransportButton};
use crate::hook::HookEvent;
use crate::icon_service::{IconKey, IconReady, IconService, JobClass};
use crate::media::{MediaEvent, MediaService, NowPlaying};

#[derive(Debug)]
pub enum AppEvent {
    Hook(HookEvent),
    Menu(MenuEvent),
    /// The icon worker finished a decode.
    Icon(IconReady),
    /// The file picker thread closed, with or without a pick.
    FilePicked {
        purpose: PickPurpose,
        path: Option<PathBuf>,
    },
    /// The media worker reported a Now Playing state or art change.
    Media(MediaEvent),
}

struct App {
    cfg: Config,
    /// Rebuilt alongside cfg (startup + reload); the one source of Menu shape.
    geo: MenuGeometry,
    cfg_raw: String,
    window: Option<Arc<Window>>,
    gfx: Option<gfx::Gfx>,
    tray: Option<TrayIcon>,
    proxy: EventLoopProxy<AppEvent>,
    icons: IconService,
    media: MediaService,
    /// Current Now Playing snapshot, or None when nothing is Playing/Paused.
    now_playing: Option<NowPlaying>,
    /// One key per Item, parallel to cfg.items. An arriving icon is matched
    /// against these rather than against the request that asked for it, so a
    /// config reload mid-decode drops the stale result instead of misplacing it.
    slot_keys: Vec<IconKey>,
    /// Window center in global screen px, valid while shown.
    center: (f64, f64),
    hover: Option<usize>,
    held: bool,
    /// The Gear zone (Hub's bottom segment) is under the cursor.
    gear_hover: bool,
    /// Some while Pinned. A Popover inside it is optional — see ADR-0005.
    pinned: Option<Pinned>,
    /// A file dialog is up: suppress the focus-loss discard until it reports back.
    in_dialog: bool,
    /// Cursor position relative to the Menu center (window px), while Pinned.
    cursor_rel: (f32, f32),
    modifiers: ModifiersState,
    /// What the Popover's icon preview is currently showing.
    popover_key: Option<IconKey>,
    /// The letter standing in for that preview; `begin_pin` shapes it as '?'.
    popover_fallback: char,
}

/// The Pinned state (ADR-0005): the Menu holds itself up and the mouse is free.
/// With no Popover open the Tiles are live — draggable, each with a remove
/// control — and the Hub carries the Dodaj slot's toggle. A Popover is modal
/// over all of that: while one is open, `popover` is Some and nothing else in
/// here is touched.
#[derive(Default)]
struct Pinned {
    popover: Option<popover::PopoverState>,
    drag: Option<Drag>,
    /// The Tile whose remove control is under the cursor.
    hover_remove: Option<usize>,
    /// The Hub's toggle is under the cursor.
    hover_toggle: bool,
    /// The Done button is under the cursor.
    hover_done: bool,
}

/// A Tile being dragged to a new Slot. Created on mouse-down over a Tile, but
/// `moved` stays false until the cursor passes the threshold — that is what
/// keeps a click (which opens the Popover) from being read as a tiny drag.
struct Drag {
    /// Item index picked up.
    from: usize,
    /// Item index it would land on if dropped now.
    to: usize,
    /// Cursor position at mouse-down, Menu-center-relative px.
    origin: (f32, f32),
    moved: bool,
}

/// How far the cursor must travel before a press becomes a drag, px.
const DRAG_THRESHOLD: f32 = 5.0;

/// The tray icon, baked in at compile time (icon concept 2a).
fn tray_icon_rgba() -> tray_icon::Icon {
    let img = image::load_from_memory(include_bytes!("../assets/icon/tray-32.png"))
        .expect("decode tray icon")
        .to_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("tray icon")
}

impl App {
    fn reload_config(&mut self) {
        let path = config::config_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        if raw == self.cfg_raw {
            return;
        }
        match config::parse_and_resync(&path, &raw) {
            Ok((cfg, raw)) => {
                self.cfg_raw = raw;
                hook::TRIGGER_XBUTTON.store(cfg.trigger_button.xbutton(), Ordering::Relaxed);
                launch::set_autostart(cfg.autostart);
                let geo = MenuGeometry::new(&cfg);
                if geo.window_size() != self.geo.window_size() {
                    if let Some(w) = &self.window {
                        let size = geo.window_size();
                        let _ = w.request_inner_size(PhysicalSize::new(size, size));
                    }
                }
                self.geo = geo;
                self.cfg = cfg;
                if let Some(g) = &mut self.gfx {
                    g.set_items(&self.cfg, self.geo);
                }
                self.request_item_icons();
            }
            Err(e) => log!(
                "config parse error ({e}); keeping previous config. \
                 In JSON a Windows path needs doubled backslashes, e.g. \"C:\\\\Tools\\\\app.exe\""
            ),
        }
    }

    /// Ask for every Tile's icon. Cached ones are applied on the spot; the rest
    /// arrive as `AppEvent::Icon` and the Tile shows its letter until they do.
    fn request_item_icons(&mut self) {
        self.slot_keys = self.cfg.items.iter().map(IconService::key_for_item).collect();
        for k in 0..self.slot_keys.len() {
            let key = self.slot_keys[k].clone();
            let slot = self.geo.slot_of_item(k);
            if let Some(Some(icon)) = self.icons.request(key, JobClass::Menu)
                && let Some(g) = &mut self.gfx
            {
                g.set_slot_icon(slot, &icon);
            }
        }
    }

    fn popover(&self) -> Option<&popover::PopoverState> {
        self.pinned.as_ref()?.popover.as_ref()
    }

    fn popover_mut(&mut self) -> Option<&mut popover::PopoverState> {
        self.pinned.as_mut()?.popover.as_mut()
    }

    fn on_icon_ready(&mut self, ready: IconReady) {
        self.icons.complete(&ready);
        let Some(icon) = &ready.icon else {
            return; // failed: the fallback letter already stands in for it
        };
        let fallback = self
            .popover()
            .map(|ps| ps.final_name().chars().next().unwrap_or('?'));
        if let Some(g) = &mut self.gfx {
            for k in 0..self.slot_keys.len() {
                if self.slot_keys[k] == ready.key {
                    g.set_slot_icon(self.geo.slot_of_item(k), icon);
                }
            }
            if self.popover_key.as_ref() == Some(&ready.key)
                && let Some(fallback) = fallback
            {
                g.set_popover_icon(Some(icon), fallback);
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// The media worker reported a state change or a finished art decode.
    /// Popover/Pinned/idle priority still lives in `gfx`; this just keeps the
    /// data current and repaints.
    fn on_media_event(&mut self, ev: MediaEvent) {
        match ev {
            MediaEvent::State(np) => {
                self.now_playing = np;
                if let Some(g) = &mut self.gfx {
                    g.set_now_playing(self.now_playing.as_ref());
                }
            }
            MediaEvent::Art { track_key, icon } => {
                // The track may have moved on while this was decoding; a stale
                // result just warms nothing instead of landing on the wrong art.
                if self.now_playing.as_ref().is_some_and(|n| n.track_key == track_key)
                    && let Some(g) = &mut self.gfx
                {
                    g.set_now_playing_art(icon.as_deref());
                }
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn on_file_picked(&mut self, purpose: PickPurpose, path: Option<PathBuf>) {
        self.in_dialog = false;
        if let Some(w) = &self.window {
            w.focus_window();
        }
        if let Some(path) = path {
            let path = path.to_string_lossy().to_string();
            if let Some(ps) = self.popover_mut() {
                match purpose {
                    PickPurpose::Icon => {
                        ps.icon_override = Some(path);
                        ps.generation += 1;
                    }
                    PickPurpose::Target => ps.apply_picked_file(&path),
                }
            }
            self.refresh_popover_icon();
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn show_menu(&mut self, x: i32, y: i32) {
        self.reload_config();
        let Some(window) = &self.window else { return };
        let size = window.inner_size().width as i32;
        let work = work_area_at(x, y);
        let px = (x - size / 2).clamp(work.left, (work.right - size).max(work.left));
        let py = (y - size / 2).clamp(work.top, (work.bottom - size).max(work.top));
        window.set_outer_position(PhysicalPosition::new(px, py));
        self.center = ((px + size / 2) as f64, (py + size / 2) as f64);
        self.hover = None;
        self.gear_hover = false;
        self.held = true;
        hook::MENU_OPEN.store(true, Ordering::Relaxed);
        window.set_visible(true);
        if let Some(g) = &mut self.gfx {
            g.begin_open();
        }
        window.request_redraw();
    }

    /// Cosmetic close: launching (if any) already happened at the call site;
    /// the window hides once the close animation ends.
    fn close_menu(&mut self, launched: Option<usize>) {
        self.held = false;
        self.hover = None;
        self.gear_hover = false;
        hook::MENU_OPEN.store(false, Ordering::Relaxed);
        if let Some(g) = &mut self.gfx {
            g.begin_close(launched);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Enter Pinned (ADR-0005): the Menu stays up, the window becomes
    /// activatable and focused, and the mouse is free. No Popover — the Tiles
    /// are live and the Hub carries the Dodaj slot's toggle.
    fn pin_bare(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        self.held = false;
        self.hover = None;
        self.gear_hover = false;
        hook::MENU_OPEN.store(false, Ordering::Relaxed);

        set_no_activate(&window, false);
        if let Some(hwnd) = hwnd_of(&window) {
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
        }
        window.focus_window();
        window.set_ime_allowed(true);
        self.pinned = Some(Pinned::default());
        window.request_redraw();
    }

    /// Open a Popover over Pinned: `Some(i)` edits that Item, `None` adds one.
    /// Entering Pinned first if we are not already there, so releasing over the
    /// Dodaj slot still goes straight to the form.
    fn open_popover(&mut self, edit: Option<usize>) {
        if self.pinned.is_none() {
            self.pin_bare();
        }
        let Some(pinned) = &mut self.pinned else { return };

        // The panel draws centered over the Menu, inside the existing window
        // bounds, expanding out of the Tile it belongs to. ADR-0002 forced this
        // (resizing a DComp swapchain reset the AMD driver); ADR-0007 removed
        // the swapchain, so growing the window is merely unnecessary now.
        let slot = match edit {
            Some(i) => self.geo.slot_of_item(i),
            None => self.geo.meta_slot().unwrap_or(0),
        };
        let origin = self.geo.tile_center(slot);
        let mut ps = popover::PopoverState::new(popover::Layout::new([0.0, 0.0]), origin);
        if let Some(item) = edit.and_then(|i| self.cfg.items.get(i)) {
            ps.load(&item.name, &item.target, item.icon.as_deref());
        }
        ps.editing = edit;
        pinned.popover = Some(ps);
        pinned.drag = None;
        pinned.hover_remove = None;
        if let Some(g) = &mut self.gfx {
            g.open_popover(edit.is_some());
        }
        self.popover_key = None;
        self.popover_fallback = '?'; // what open_popover just shaped
        self.refresh_popover_icon();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Close the Popover but stay Pinned — the Menu shows the result of what
    /// just happened instead of vanishing with it.
    ///
    /// ponytail: the panel disappears on the frame it closes rather than
    /// collapsing back into its Tile. The collapse spring exists, but nothing
    /// drives it: `draw` reads the layout out of the live `PopoverState`, and
    /// that is gone the moment we return. To animate it out, gfx would have to
    /// keep the last panel rect and origin alive until the spring settles.
    fn close_popover(&mut self) {
        if let Some(p) = &mut self.pinned {
            p.popover = None;
        }
        if let Some(g) = &mut self.gfx {
            g.close_popover();
        }
        self.popover_key = None;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Leave Pinned; `and_close` also plays the normal close animation.
    fn unpin(&mut self, and_close: bool) {
        if let Some(window) = &self.window {
            set_no_activate(window, true);
            window.set_ime_allowed(false);
        }
        self.pinned = None;
        if let Some(g) = &mut self.gfx {
            g.close_popover();
        }
        if and_close {
            self.close_menu(None);
        }
    }

    /// The one write path for Item mutations (ADR-0006): apply `f` to the
    /// config the app holds, save it, and rebuild everything that depends on
    /// the Item list. A future undo hooks in right here.
    fn mutate(&mut self, f: impl FnOnce(&mut Config)) {
        f(&mut self.cfg);
        match config::save(&self.cfg) {
            Ok(raw) => self.cfg_raw = raw,
            Err(e) => log!("could not save config: {e}"),
        }
        self.geo = MenuGeometry::new(&self.cfg);
        if let Some(g) = &mut self.gfx {
            g.set_items(&self.cfg, self.geo);
        }
        self.request_item_icons();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Flip the Dodaj slot's `hidden` — what the Hub's toggle does. `position`
    /// is deliberately untouched, so showing it again puts it back.
    fn toggle_add_slot(&mut self) {
        self.mutate(|cfg| cfg.add_slot.hidden = !cfg.add_slot.hidden);
    }

    /// Cursor moved while the window has focus. A Popover, when open, is modal
    /// over Pinned: it takes the cursor and the Tiles stay inert.
    fn cursor_moved(&mut self, rel: (f32, f32)) {
        let geo = self.geo;
        let Some(p) = &mut self.pinned else { return };
        if let Some(ps) = &mut p.popover {
            ps.hover = ps.layout.hit(rel);
            return;
        }
        if let Some(d) = &mut p.drag {
            if !d.moved {
                let (dx, dy) = (rel.0 - d.origin.0, rel.1 - d.origin.1);
                d.moved = dx * dx + dy * dy >= DRAG_THRESHOLD * DRAG_THRESHOLD;
            }
            if d.moved {
                // The drop target is the sector under the cursor, reusing the
                // Menu's own hit-test; the meta sector clamps to an Item.
                d.to = geo
                    .hovered_slot((rel.0 as f64, rel.1 as f64), (0.0, 0.0))
                    .map_or(d.to, |slot| geo.drop_index(slot));
            }
            p.hover_remove = None;
            p.hover_toggle = false;
            p.hover_done = false;
        } else {
            p.hover_remove = (0..geo.slot_count())
                .find(|&k| geo.item_at(k).is_some() && geo.on_remove(rel, k));
            p.hover_toggle = geo.on_toggle(rel);
            p.hover_done = geo.on_done(rel);
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Left button pressed while Pinned. Returns the Popover action to run, if
    /// the press landed on one.
    fn pinned_press(&mut self, rel: (f32, f32)) -> Option<popover::Action> {
        let geo = self.geo;
        let in_menu = rel.0 * rel.0 + rel.1 * rel.1 < geo.scrim_r() * geo.scrim_r();

        if let Some(ps) = self.popover_mut() {
            return match ps.layout.hit(rel) {
                Some(el) => ps.on_click(el),
                None => {
                    let in_panel = ps.layout.panel.contains(rel);
                    (!in_menu && !in_panel).then_some(popover::Action::Discard)
                }
            };
        }

        // The toggle wins over everything else in the Hub.
        if geo.on_toggle(rel) {
            self.toggle_add_slot();
            return None;
        }
        // Done: leave Pinned. Same segment the Gear zone used to get here.
        if geo.on_done(rel) {
            return Some(popover::Action::Discard);
        }
        // A remove control swallows the press: it never starts a drag.
        if let Some(k) = (0..geo.slot_count())
            .find(|&k| geo.item_at(k).is_some() && geo.on_remove(rel, k))
        {
            self.remove_slot(k);
            return None;
        }
        let slot = geo
            .hovered_slot((rel.0 as f64, rel.1 as f64), (0.0, 0.0))
            .filter(|_| in_menu);
        // The Dodaj slot opens an empty form; it is not draggable.
        if slot.is_some() && slot == geo.meta_slot() {
            self.open_popover(None);
            return None;
        }
        // On an Item Tile: arm a drag. Whether this turns out to be a drag or a
        // click is decided on release, by how far the cursor travelled.
        if let Some(i) = slot.and_then(|k| geo.item_at(k))
            && let Some(p) = &mut self.pinned
        {
            p.drag = Some(Drag {
                from: i,
                to: i,
                origin: rel,
                moved: false,
            });
            return None;
        }
        // Clicking outside the Menu leaves Pinned entirely.
        (!in_menu).then_some(popover::Action::Discard)
    }

    /// Left button released while Pinned: finish a drag, or treat it as a click
    /// on the Tile and open its Popover.
    fn pinned_release(&mut self) {
        let Some(p) = &mut self.pinned else { return };
        let Some(d) = p.drag.take() else { return };
        if !d.moved {
            self.open_popover(Some(d.from));
        } else if d.to != d.from {
            // Move the springs with the Tile before the rebuild, or it snaps
            // back to where its Slot used to be.
            let (from_slot, to_slot) = (self.geo.slot_of_item(d.from), self.geo.slot_of_item(d.to));
            if let Some(g) = &mut self.gfx {
                g.reorder_slots(from_slot, to_slot);
            }
            self.mutate(|cfg| {
                if d.from < cfg.items.len() && d.to < cfg.items.len() {
                    let item = cfg.items.remove(d.from);
                    cfg.items.insert(d.to, item);
                }
            });
        }
    }

    /// Start removing Slot `k`'s Item. The Item stays in the config until the
    /// pop finishes — `Tick::remove_done` is what actually drops it.
    fn remove_slot(&mut self, k: usize) {
        if self.geo.item_at(k).is_some()
            && let Some(g) = &mut self.gfx
        {
            g.begin_remove(k);
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn popover_action(&mut self, action: popover::Action) {
        match action {
            // Closing a Popover returns to Pinned; it does not dismiss the Menu.
            popover::Action::Discard => self.close_popover(),
            popover::Action::Commit => {
                if let Some(ps) = self.popover() {
                    // Copy the picked image into the Icon Library now, so the
                    // Item keeps its icon when the original is moved or deleted.
                    // Editing an Item re-imports the icon it already had, which
                    // is a no-op: library names are content hashes (ADR-0003).
                    let icon = ps.icon_override.as_ref().map(|p| {
                        match icons::import_to_library(std::path::Path::new(p)) {
                            Ok(stored) => stored.to_string_lossy().to_string(),
                            Err(e) => {
                                log!("could not copy icon {p} into the library: {e}");
                                p.clone()
                            }
                        }
                    });
                    let item = config::Item {
                        name: ps.final_name(),
                        target: ps.target.text.trim().to_string(),
                        icon,
                    };
                    let editing = ps.editing;
                    self.mutate(|cfg| match editing.filter(|&i| i < cfg.items.len()) {
                        Some(i) => cfg.items[i] = item,
                        None => cfg.items.push(item),
                    });
                }
                self.close_popover();
            }
            popover::Action::Browse | popover::Action::BrowseIcon => {
                if self.in_dialog {
                    return;
                }
                let Some(hwnd) = self.window.as_deref().and_then(hwnd_of) else {
                    return;
                };
                let purpose = if action == popover::Action::BrowseIcon {
                    PickPurpose::Icon
                } else {
                    PickPurpose::Target
                };
                // The picker runs on its own thread and reports back as
                // AppEvent::FilePicked; the event loop keeps running behind it,
                // which is what keeps in_dialog covering the dialog's real
                // lifetime. See ADR-0004.
                self.in_dialog = true;
                dialog::pick_file_async(hwnd.0 as isize, purpose, self.proxy.clone());
            }
        }
    }

    fn popover_key(&mut self, event: &KeyEvent) {
        use winit::keyboard::{Key as WKey, NamedKey};
        let mapped = match &event.logical_key {
            WKey::Named(NamedKey::Escape) => Some(popover::Key::Escape),
            WKey::Named(NamedKey::Enter) => Some(popover::Key::Enter),
            WKey::Named(NamedKey::Tab) => Some(popover::Key::Tab),
            WKey::Named(NamedKey::Backspace) => Some(popover::Key::Backspace),
            WKey::Named(NamedKey::Delete) => Some(popover::Key::Delete),
            WKey::Named(NamedKey::ArrowLeft) => Some(popover::Key::Left),
            WKey::Named(NamedKey::ArrowRight) => Some(popover::Key::Right),
            WKey::Named(NamedKey::Home) => Some(popover::Key::Home),
            WKey::Named(NamedKey::End) => Some(popover::Key::End),
            _ => None,
        };
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let mut action = None;
        if let Some(ps) = self.popover_mut() {
            if let Some(k) = mapped {
                action = ps.on_key(k);
            } else if ctrl && !alt {
                // Plain Ctrl shortcuts. AltGr arrives as Ctrl+Alt and falls
                // through to the text branch, so Polish diacritics still type.
                if matches!(&event.logical_key, WKey::Character(c) if c.eq_ignore_ascii_case("v"))
                    && let Some(text) = dialog::clipboard_text()
                {
                    ps.insert(&text);
                }
            } else if let Some(text) = &event.text {
                let s: String = text.chars().filter(|c| !c.is_control()).collect();
                if !s.is_empty() {
                    ps.insert(&s);
                }
            }
        }
        self.refresh_popover_icon();
        if let Some(a) = action {
            self.popover_action(a);
        }
    }

    /// Point the Popover's icon preview at whatever the fields describe now.
    /// A cached icon appears immediately; anything else shows the fallback
    /// letter until the worker reports back.
    fn refresh_popover_icon(&mut self) {
        let Some(ps) = self.popover() else { return };
        let key = IconService::key_for_popover(&ps.target.text, ps.icon_override.as_deref());
        let fallback = ps.final_name().chars().next().unwrap_or('?');
        if key != self.popover_key {
            self.popover_key = key.clone();
            self.popover_fallback = fallback;
            let icon = key
                .and_then(|k| self.icons.request(k, JobClass::Preview))
                .flatten();
            if let Some(g) = &mut self.gfx {
                g.set_popover_icon(icon.as_deref(), fallback);
            }
        } else if fallback != self.popover_fallback {
            // Typing a name changes the letter but not the icon.
            self.popover_fallback = fallback;
            if let Some(g) = &mut self.gfx {
                g.set_popover_fallback(fallback);
            }
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let menu = Menu::new();
        let _ = menu.append(&MenuItem::with_id("edit", "Edit config", true, None));
        let _ = menu.append(&MenuItem::with_id("quit", "Quit", true, None));
        match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("sideQM — quick menu")
            .with_icon(tray_icon_rgba())
            .build()
        {
            Ok(tray) => {
                self.tray = Some(tray);
                launch::promote_tray_icon();
            }
            Err(e) => log!("tray icon failed: {e}"),
        }
        let size = self.geo.window_size();
        let attrs = Window::default_attributes()
            .with_title("sideQM")
            .with_decorations(false)
            // No .with_transparent(true): that asks DWM for blur-behind, which
            // the DirectComposition path needed. A layered window carries its
            // own per-pixel alpha, and the two do not mix (ADR-0007).
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(PhysicalSize::new(size, size))
            .with_skip_taskbar(true);
        let window = Arc::new(event_loop.create_window(attrs).expect("window creation"));
        set_no_activate(&window, true);
        if let Some(hwnd) = hwnd_of(&window) {
            self.gfx = Some(gfx::Gfx::new(hwnd, &self.cfg, self.geo));
        } else {
            log!("no HWND for the window; the menu cannot render");
        }
        self.window = Some(window);
        self.request_item_icons();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        let event = match event {
            AppEvent::Hook(ev) => ev,
            AppEvent::Menu(ev) => {
                match ev.id().0.as_str() {
                    "edit" => launch::open(&config::config_path().to_string_lossy()),
                    "quit" => event_loop.exit(),
                    _ => {}
                }
                return;
            }
            AppEvent::Icon(ready) => {
                self.on_icon_ready(ready);
                return;
            }
            AppEvent::FilePicked { purpose, path } => {
                self.on_file_picked(purpose, path);
                return;
            }
            AppEvent::Media(ev) => {
                self.on_media_event(ev);
                return;
            }
        };
        match event {
            HookEvent::TriggerDown { x, y } => {
                // A file picker owns the Popover right now; re-summoning the
                // Menu would strand the pick that is still coming back.
                if self.in_dialog {
                    return;
                }
                // Pressing the Trigger while Pinned discards the Popover and
                // starts a fresh press-and-hold Menu at the cursor.
                if self.pinned.is_some() {
                    self.unpin(false);
                }
                self.show_menu(x, y);
            }
            HookEvent::Move { x, y } => {
                if self.held {
                    let cur = (x as f64, y as f64);
                    self.hover = self.geo.hovered_slot(cur, self.center);
                    self.gear_hover =
                        self.hover.is_none() && self.geo.in_gear_zone(cur, self.center);
                }
            }
            HookEvent::TriggerUp { x: _, y: _ } => {
                if self.held {
                    println!("trigger up, hover = {:?}", self.hover);
                    match self.hover.take() {
                        // Release over Dodaj: pin the Menu, open the Popover.
                        Some(k) if self.geo.meta_slot() == Some(k) => self.open_popover(None),
                        Some(k) => {
                            if let Some(item) =
                                self.geo.item_at(k).and_then(|i| self.cfg.items.get(i))
                            {
                                launch::open(&item.target);
                            }
                            self.close_menu(Some(k));
                        }
                        // ADR-0005: the Gear zone enters Pinned with no Popover
                        // open. Opening config.json by hand is the tray's job.
                        None if self.gear_hover => self.pin_bare(),
                        None => self.close_menu(None),
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Debug aid: show the menu without any hook involvement.
        if std::env::var_os("SIDEQM_AUTOSHOW").is_some() && !self.held && self.pinned.is_none() {
            self.show_menu(960, 540);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gfx {
                    g.resize(size.width, size.height);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::Focused(false) => {
                // Clicking away (or alt-tab) while Pinned discards — unless the
                // focus went to our own modal file dialog.
                if self.pinned.is_some() && !self.in_dialog {
                    self.unpin(true);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(w) = &self.window else { return };
                let half = w.inner_size().width as f64 / 2.0;
                let rel = ((position.x - half) as f32, (position.y - half) as f32);
                self.cursor_rel = rel;
                self.cursor_moved(rel);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                // Transport buttons (ADR-0008): a left click, not a Trigger
                // release, and it works during the held-Trigger Menu — the
                // Trigger is a side button, thumb-held, leaving the index
                // finger free. Held-only, not Pinned: Now Playing (and these
                // buttons with it) never draws while Pinned — the Hub belongs
                // to the Dodaj toggle there, which sits in the same screen
                // spot the PlayPause button would, and must win the click.
                if self.now_playing.is_some()
                    && self.held
                    && let Some(btn) = self.geo.transport_button(self.cursor_rel)
                {
                    self.media.send(match btn {
                        TransportButton::Prev => media::Command::Prev,
                        TransportButton::PlayPause => media::Command::PlayPause,
                        TransportButton::Next => media::Command::Next,
                    });
                    return;
                }
                if self.pinned.is_some() {
                    let rel = self.cursor_rel;
                    // Clicking away leaves Pinned altogether; anything a
                    // Popover claims is handled inside it.
                    match self.pinned_press(rel) {
                        Some(popover::Action::Discard) if self.popover().is_none() => {
                            self.unpin(true)
                        }
                        Some(a) => self.popover_action(a),
                        None => {}
                    }
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                if self.pinned.is_some() {
                    self.pinned_release();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                if self.popover().is_some() {
                    self.popover_key(&event);
                } else if self.pinned.is_some()
                    && event.logical_key == winit::keyboard::Key::Named(NamedKey::Escape)
                {
                    // No Popover to close, so Escape leaves Pinned.
                    self.unpin(true);
                }
            }
            WindowEvent::Ime(Ime::Commit(s)) => {
                if let Some(ps) = self.popover_mut() {
                    ps.insert(&s);
                    self.refresh_popover_icon();
                }
            }
            WindowEvent::DroppedFile(path) => {
                // Only while Pinned: the press-and-hold Menu holds the mouse
                // button down, so there is no hand free to drag anything onto it.
                if let Some(ps) = self.popover_mut() {
                    let path = path.to_string_lossy().to_string();
                    match popover::classify_drop(&path) {
                        popover::DropKind::Image => {
                            ps.icon_override = Some(path);
                            ps.generation += 1;
                        }
                        popover::DropKind::Target => ps.apply_picked_file(&path),
                    }
                    self.refresh_popover_icon();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                let mut removed = None;
                if let (Some(g), Some(w)) = (&mut self.gfx, &self.window) {
                    let pinned = self.pinned.as_ref();
                    let drag = pinned.and_then(|p| p.drag.as_ref()).filter(|d| d.moved).map(|d| {
                        gfx::DragView {
                            from: d.from,
                            to: d.to,
                            cursor: [self.cursor_rel.0, self.cursor_rel.1],
                        }
                    });
                    let view = gfx::MenuView {
                        hover: self.hover,
                        gear_hover: self.gear_hover,
                        popover: pinned.and_then(|p| p.popover.as_ref()),
                        editing: pinned.is_some(),
                        hover_remove: pinned.and_then(|p| p.hover_remove),
                        hover_toggle: pinned.is_some_and(|p| p.hover_toggle),
                        hover_done: pinned.is_some_and(|p| p.hover_done),
                        add_hidden: self.cfg.add_slot.hidden,
                        drag,
                        now_playing: self.now_playing.as_ref(),
                        cursor_rel: [self.cursor_rel.0, self.cursor_rel.1],
                    };
                    let tick = g.tick_render(&view);
                    removed = tick.remove_done;
                    if tick.just_closed {
                        w.set_visible(false);
                    } else if tick.request_frame {
                        w.request_redraw();
                    }
                }
                // The pop finished: now the Item actually goes.
                if let Some(slot) = removed
                    && let Some(i) = self.geo.item_at(slot)
                {
                    if let Some(g) = &mut self.gfx {
                        g.drop_slot(slot);
                    }
                    self.mutate(|cfg| {
                        if i < cfg.items.len() {
                            cfg.items.remove(i);
                        }
                    });
                }
            }
            _ => {}
        }
    }
}

fn work_area_at(x: i32, y: i32) -> RECT {
    unsafe {
        let hmon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut mi);
        mi.rcWork
    }
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return None;
    };
    Some(HWND(h.hwnd.get() as *mut _))
}

/// Toggle WS_EX_NOACTIVATE (always keeping WS_EX_TOOLWINDOW and WS_EX_LAYERED):
/// on for the press-and-hold Menu (never steals focus), off while Pinned (the
/// Popover needs keyboard focus).
///
/// WS_EX_LAYERED is what makes UpdateLayeredWindow legal, and it is OR'd in on
/// every call rather than set once — clearing it would discard the window's
/// contents and leave the Menu invisible until the next frame.
fn set_no_activate(window: &Window, on: bool) {
    let Some(hwnd) = hwnd_of(window) else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let na = WS_EX_NOACTIVATE.0 as isize;
        let ex = if on { ex | na } else { ex & !na };
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex | WS_EX_TOOLWINDOW.0 as isize | WS_EX_LAYERED.0 as isize,
        );
    }
}

fn main() {
    // Single instance: second launch bails immediately.
    let _mutex = unsafe { CreateMutexW(None, true, w!("sideQM-single-instance")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        eprintln!("sideQM is already running");
        return;
    }

    logging::init();

    // COM apartment for shell calls on this thread; S_FALSE (already
    // initialized) is fine, so the result is deliberately ignored. winit's own
    // OleInitialize (which is what makes file drops work) is happy to join an
    // existing STA.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let (cfg, cfg_raw) = config::load();
    hook::TRIGGER_XBUTTON.store(cfg.trigger_button.xbutton(), Ordering::Relaxed);
    launch::set_autostart(cfg.autostart);

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("event loop");
    hook::install(event_loop.create_proxy());
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |ev| {
        let _ = menu_proxy.send_event(AppEvent::Menu(ev));
    }));

    let geo = MenuGeometry::new(&cfg);
    let proxy = event_loop.create_proxy();
    let mut app = App {
        cfg,
        geo,
        cfg_raw,
        window: None,
        gfx: None,
        tray: None,
        icons: IconService::new(proxy.clone()),
        media: MediaService::new(proxy.clone()),
        now_playing: None,
        proxy,
        slot_keys: Vec::new(),
        center: (0.0, 0.0),
        hover: None,
        held: false,
        gear_hover: false,
        pinned: None,
        in_dialog: false,
        cursor_rel: (0.0, 0.0),
        modifiers: ModifiersState::default(),
        popover_key: None,
        popover_fallback: '?',
    };
    event_loop.run_app(&mut app).expect("event loop run");
}
