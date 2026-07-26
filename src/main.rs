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
mod popover;

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
    GWL_EXSTYLE, GetWindowLongPtrW, SetForegroundWindow, SetWindowLongPtrW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW,
};
use windows::core::w;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId, WindowLevel};

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::config::Config;
use crate::dialog::PickPurpose;
use crate::geometry::MenuGeometry;
use crate::hook::HookEvent;
use crate::icon_service::{IconKey, IconReady, IconService, JobClass};

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
    /// Pinned state: the add-item Popover is open; Some while it lives.
    pinned: Option<popover::PopoverState>,
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

/// 32x32 mint filled circle, the tray icon.
fn tray_icon_rgba() -> tray_icon::Icon {
    const N: i32 = 32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside = (dx * dx + dy * dy).sqrt() <= 14.0;
            px.extend_from_slice(if inside {
                &[0x5D, 0xCA, 0xA5, 0xff]
            } else {
                &[0, 0, 0, 0]
            });
        }
    }
    tray_icon::Icon::from_rgba(px, N as u32, N as u32).expect("tray icon")
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
                let geo = MenuGeometry::new(&cfg.appearance, cfg.items.len());
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
            if let Some(Some(icon)) = self.icons.request(key, JobClass::Menu)
                && let Some(g) = &mut self.gfx
            {
                g.set_slot_icon(k, &icon);
            }
        }
    }

    fn on_icon_ready(&mut self, ready: IconReady) {
        self.icons.complete(&ready);
        let Some(icon) = &ready.icon else {
            return; // failed: the fallback letter already stands in for it
        };
        if let Some(g) = &mut self.gfx {
            for k in 0..self.slot_keys.len() {
                if self.slot_keys[k] == ready.key {
                    g.set_slot_icon(k, icon);
                }
            }
            if self.popover_key.as_ref() == Some(&ready.key)
                && let Some(ps) = &self.pinned
            {
                let fallback = ps.final_name().chars().next().unwrap_or('?');
                g.set_popover_icon(Some(icon), fallback);
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
            if let Some(ps) = &mut self.pinned {
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

    /// Enter Pinned: the Menu stays up, the window becomes activatable and
    /// focused, and the add-item Popover expands out of the Dodaj Tile.
    fn pin(&mut self) {
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

        // ADR-0002: the window never resizes while the swapchain lives (the AMD
        // driver resets on ResizeBuffers of a DComp swapchain), so the panel
        // draws centered over the Menu, inside the existing window bounds.
        let a = self.geo.slot_angle(self.geo.meta_slot());
        let origin = [a.cos() * self.geo.rest_r(), a.sin() * self.geo.rest_r()];
        self.pinned = Some(popover::PopoverState::new(
            popover::Layout::new([0.0, 0.0]),
            origin,
        ));
        if let Some(g) = &mut self.gfx {
            g.begin_pin();
        }
        self.popover_key = None;
        self.popover_fallback = '?'; // what begin_pin just shaped
        self.refresh_popover_icon();
        window.request_redraw();
    }

    /// Leave Pinned; `and_close` also plays the normal close animation.
    fn unpin(&mut self, and_close: bool) {
        if let Some(window) = &self.window {
            set_no_activate(window, true);
            window.set_ime_allowed(false);
        }
        self.pinned = None;
        if let Some(g) = &mut self.gfx {
            g.end_pin();
        }
        if and_close {
            self.close_menu(None);
        }
    }

    fn popover_action(&mut self, action: popover::Action) {
        match action {
            popover::Action::Discard => self.unpin(true),
            popover::Action::Commit => {
                if let Some(ps) = &self.pinned {
                    // Copy the picked image into the Icon Library now, so the
                    // Item keeps its icon when the original is moved or deleted.
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
                    if let Err(e) = config::append_item(item, &self.cfg) {
                        log!("could not save the new item: {e}");
                    }
                }
                self.unpin(true);
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
        if let Some(ps) = &mut self.pinned {
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
        let Some(ps) = &self.pinned else { return };
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
            .with_transparent(true)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(PhysicalSize::new(size, size))
            .with_skip_taskbar(true)
            // DirectComposition presents under the GDI redirection bitmap; drop it
            // or the swapchain is invisible.
            .with_no_redirection_bitmap(true);
        let window = Arc::new(event_loop.create_window(attrs).expect("window creation"));
        set_no_activate(&window, true);
        self.gfx = Some(gfx::Gfx::new(window.clone(), &self.cfg, self.geo));
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
                        Some(k) if k == self.geo.meta_slot() => self.pin(),
                        Some(k) => {
                            if let Some(item) = self.cfg.items.get(k) {
                                launch::open(&item.target);
                            }
                            self.close_menu(Some(k));
                        }
                        // Gear zone: the old "edit the JSON" path.
                        None if self.gear_hover => {
                            launch::open(&config::config_path().to_string_lossy());
                            self.close_menu(None);
                        }
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
                if let (Some(ps), Some(w)) = (&mut self.pinned, &self.window) {
                    let half = w.inner_size().width as f64 / 2.0;
                    let rel = ((position.x - half) as f32, (position.y - half) as f32);
                    self.cursor_rel = rel;
                    ps.hover = ps.layout.hit(rel);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(ps) = &mut self.pinned {
                    let rel = self.cursor_rel;
                    let action = match ps.layout.hit(rel) {
                        Some(el) => ps.on_click(el),
                        None => {
                            let in_menu = rel.0 * rel.0 + rel.1 * rel.1
                                < self.geo.scrim_r() * self.geo.scrim_r();
                            let in_panel = ps.layout.panel.contains(rel);
                            (!in_menu && !in_panel).then_some(popover::Action::Discard)
                        }
                    };
                    if let Some(a) = action {
                        self.popover_action(a);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.pinned.is_some() && event.state == ElementState::Pressed {
                    self.popover_key(&event);
                }
            }
            WindowEvent::Ime(Ime::Commit(s)) => {
                if let Some(ps) = &mut self.pinned {
                    ps.insert(&s);
                    self.refresh_popover_icon();
                }
            }
            WindowEvent::DroppedFile(path) => {
                // Only while Pinned: the press-and-hold Menu holds the mouse
                // button down, so there is no hand free to drag anything onto it.
                if let Some(ps) = &mut self.pinned {
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
                if let (Some(g), Some(w)) = (&mut self.gfx, &self.window) {
                    let view = gfx::MenuView {
                        hover: self.hover,
                        gear_hover: self.gear_hover,
                        popover: self.pinned.as_ref(),
                    };
                    let tick = g.tick_render(&view);
                    if tick.just_closed {
                        w.set_visible(false);
                    } else if tick.request_frame {
                        w.request_redraw();
                    }
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

/// Toggle WS_EX_NOACTIVATE (always keeping WS_EX_TOOLWINDOW): on for the
/// press-and-hold Menu (never steals focus), off while Pinned (the Popover
/// needs keyboard focus).
fn set_no_activate(window: &Window, on: bool) {
    let Some(hwnd) = hwnd_of(window) else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let na = WS_EX_NOACTIVATE.0 as isize;
        let ex = if on { ex | na } else { ex & !na };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_TOOLWINDOW.0 as isize);
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

    let geo = MenuGeometry::new(&cfg.appearance, cfg.items.len());
    let proxy = event_loop.create_proxy();
    let mut app = App {
        cfg,
        geo,
        cfg_raw,
        window: None,
        gfx: None,
        tray: None,
        icons: IconService::new(proxy.clone()),
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
