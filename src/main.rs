#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod config;
mod dialog;
mod geometry;
mod gfx;
mod hook;
mod icons;
mod launch;
mod popover;

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
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::ModifiersState;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId, WindowLevel};

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::config::Config;
use crate::geometry::MenuGeometry;
use crate::hook::HookEvent;

#[derive(Debug)]
pub enum AppEvent {
    Hook(HookEvent),
    Menu(MenuEvent),
}

struct App {
    cfg: Config,
    /// Rebuilt alongside cfg (startup + reload); the one source of Menu shape.
    geo: MenuGeometry,
    cfg_raw: String,
    window: Option<Arc<Window>>,
    gfx: Option<gfx::Gfx>,
    tray: Option<TrayIcon>,
    /// Window center in global screen px, valid while shown.
    center: (f64, f64),
    hover: Option<usize>,
    held: bool,
    /// The Gear zone (Hub's bottom segment) is under the cursor.
    gear_hover: bool,
    /// Pinned state: the add-item Popover is open; Some while it lives.
    pinned: Option<popover::PopoverState>,
    /// A modal file dialog is up: suppress the focus-loss discard.
    in_dialog: bool,
    /// Cursor position relative to the Menu center (window px), while Pinned.
    cursor_rel: (f32, f32),
    modifiers: ModifiersState,
    /// (target, icon_override) last probed for the Popover's icon preview.
    icon_probe: Option<(String, Option<String>)>,
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
            }
            Err(e) => eprintln!("sideQM: config parse error ({e}); keeping previous config"),
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

        // Panel placement: radially outward from the Dodaj Tile, kept on-screen.
        let a = self.geo.slot_angle(self.geo.meta_slot());
        let (dx, dy) = (a.cos(), a.sin());
        let support = dx.abs() * popover::PANEL_W / 2.0 + dy.abs() * popover::PANEL_H / 2.0;
        let dist = self.geo.scrim_r() + popover::GAP + support;
        let work = work_area_at(self.center.0 as i32, self.center.1 as i32);
        let (pw2, ph2) = (popover::PANEL_W / 2.0, popover::PANEL_H / 2.0);
        let sx = popover::clamp_panel_axis(
            self.center.0 as f32 + dx * dist,
            pw2,
            work.left as f32 + 8.0,
            work.right as f32 - 8.0,
        );
        let sy = popover::clamp_panel_axis(
            self.center.1 as f32 + dy * dist,
            ph2,
            work.top as f32 + 8.0,
            work.bottom as f32 - 8.0,
        );
        let rel = [sx - self.center.0 as f32, sy - self.center.1 as f32];

        // Grow the window symmetrically so the panel fits; symmetric growth
        // keeps the drawn Menu center on the same screen pixel.
        let cur = window.inner_size().width;
        let reach = (rel[0].abs() + pw2).max(rel[1].abs() + ph2) + 8.0;
        let need = ((reach * 2.0).ceil() as u32 + 1) & !1;
        if need > cur {
            if let Ok(pos) = window.outer_position() {
                let delta = ((need - cur) / 2) as i32;
                window.set_outer_position(PhysicalPosition::new(pos.x - delta, pos.y - delta));
            }
            let _ = window.request_inner_size(PhysicalSize::new(need, need));
        }

        set_no_activate(&window, false);
        if let Some(hwnd) = hwnd_of(&window) {
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
        }
        window.focus_window();
        window.set_ime_allowed(true);

        let origin = [dx * self.geo.rest_r(), dy * self.geo.rest_r()];
        self.pinned = Some(popover::PopoverState::new(
            popover::Layout::new(rel),
            origin,
        ));
        if let Some(g) = &mut self.gfx {
            g.begin_pin();
        }
        self.icon_probe = None;
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
                    let item = config::Item {
                        name: ps.final_name(),
                        target: ps.target.text.trim().to_string(),
                        icon: ps.icon_override.clone(),
                    };
                    if let Err(e) = config::append_item(item, &self.cfg) {
                        eprintln!("sideQM: could not save the new item: {e}");
                    }
                }
                self.unpin(true);
            }
            popover::Action::Browse | popover::Action::BrowseIcon => {
                let images = action == popover::Action::BrowseIcon;
                let Some(hwnd) = self.window.as_deref().and_then(hwnd_of) else {
                    return;
                };
                // Blocking modal dialog; in_dialog suppresses the Focused(false)
                // discard that the dialog taking focus would otherwise fire.
                self.in_dialog = true;
                let picked = dialog::pick_file(hwnd, images);
                self.in_dialog = false;
                if let Some(w) = &self.window {
                    w.focus_window();
                }
                if let Some(path) = picked {
                    let path = path.to_string_lossy().to_string();
                    if let Some(ps) = &mut self.pinned {
                        if images {
                            ps.icon_override = Some(path);
                            ps.generation += 1;
                        } else {
                            ps.apply_picked_file(&path);
                        }
                    }
                    self.refresh_popover_icon();
                }
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

    /// Re-extract the Popover's icon preview when its inputs changed.
    fn refresh_popover_icon(&mut self) {
        let Some(ps) = &self.pinned else { return };
        let key = (ps.target.text.trim().to_string(), ps.icon_override.clone());
        if self.icon_probe.as_ref() == Some(&key) {
            return;
        }
        let icon = if key.0.is_empty() && key.1.is_none() {
            None
        } else {
            icons::icon_for(&config::Item {
                name: String::new(),
                target: key.0.clone(),
                icon: key.1.clone(),
            })
        };
        let fallback = ps.final_name().chars().next().unwrap_or('?');
        if let Some(g) = &mut self.gfx {
            g.set_popover_icon(icon.as_ref(), fallback);
        }
        self.icon_probe = Some(key);
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
            Err(e) => eprintln!("sideQM: tray icon failed: {e}"),
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
        };
        match event {
            HookEvent::TriggerDown { x, y } => {
                println!("trigger down at ({x}, {y})");
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
                        // Shed any Pinned-time growth while nobody's looking.
                        let size = self.geo.window_size();
                        if w.inner_size().width != size {
                            let _ = w.request_inner_size(PhysicalSize::new(size, size));
                        }
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

    // COM apartment for the Popover's IFileOpenDialog; S_FALSE (already
    // initialized) is fine, so the result is deliberately ignored.
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
    let mut app = App {
        cfg,
        geo,
        cfg_raw,
        window: None,
        gfx: None,
        tray: None,
        center: (0.0, 0.0),
        hover: None,
        held: false,
        gear_hover: false,
        pinned: None,
        in_dialog: false,
        cursor_rel: (0.0, 0.0),
        modifiers: ModifiersState::default(),
        icon_probe: None,
    };
    event_loop.run_app(&mut app).expect("event loop run");
}
