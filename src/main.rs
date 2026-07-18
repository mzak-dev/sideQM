#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod gfx;
mod hook;
mod icons;
mod launch;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, HWND, POINT, RECT, ERROR_ALREADY_EXISTS};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId, WindowLevel};

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::config::Config;
use crate::hook::HookEvent;

#[derive(Debug)]
pub enum AppEvent {
    Hook(HookEvent),
    Menu(MenuEvent),
}

struct App {
    cfg: Config,
    cfg_raw: String,
    window: Option<Arc<Window>>,
    gfx: Option<gfx::Gfx>,
    tray: Option<TrayIcon>,
    /// Window center in global screen px, valid while shown.
    center: (f64, f64),
    hover: Option<usize>,
    held: bool,
}

/// 32x32 green filled circle, the tray icon.
fn tray_icon_rgba() -> tray_icon::Icon {
    const N: i32 = 32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let inside = (dx * dx + dy * dy).sqrt() <= 14.0;
            px.extend_from_slice(if inside { &[0x2e, 0xcc, 0x71, 0xff] } else { &[0, 0, 0, 0] });
        }
    }
    tray_icon::Icon::from_rgba(px, N as u32, N as u32).expect("tray icon")
}

impl App {
    /// Total slots = configured items + the edit item (always last, at 6 o'clock).
    fn total_slots(&self) -> usize {
        self.cfg.items.len() + 1
    }

    fn reload_config(&mut self) {
        let path = config::config_path();
        let Ok(raw) = std::fs::read_to_string(&path) else { return };
        if raw == self.cfg_raw {
            return;
        }
        self.cfg_raw = raw.clone();
        match config::parse(&raw) {
            Ok(cfg) => {
                hook::TRIGGER_XBUTTON.store(cfg.trigger_button.xbutton(), Ordering::Relaxed);
                launch::set_autostart(cfg.autostart);
                let new_size = gfx::window_size(cfg.appearance.radius_px);
                if new_size != gfx::window_size(self.cfg.appearance.radius_px) {
                    if let Some(w) = &self.window {
                        let _ = w.request_inner_size(PhysicalSize::new(new_size, new_size));
                    }
                }
                self.cfg = cfg;
                if let Some(g) = &mut self.gfx {
                    g.set_items(&self.cfg);
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
        self.held = true;
        hook::MENU_OPEN.store(true, Ordering::Relaxed);
        window.set_visible(true);
        window.request_redraw();
    }

    fn hide_menu_and_act(&mut self) {
        self.held = false;
        hook::MENU_OPEN.store(false, Ordering::Relaxed);
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        if let Some(k) = self.hover.take() {
            if k == self.cfg.items.len() {
                launch::open(&config::config_path().to_string_lossy());
            } else if let Some(item) = self.cfg.items.get(k) {
                launch::open(&item.target);
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
            Ok(tray) => self.tray = Some(tray),
            Err(e) => eprintln!("sideQM: tray icon failed: {e}"),
        }
        let size = gfx::window_size(self.cfg.appearance.radius_px);
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
        apply_no_activate(&window);
        self.gfx = Some(gfx::Gfx::new(window.clone(), &self.cfg));
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
                self.show_menu(x, y);
            }
            HookEvent::Move { x, y } => {
                if self.held {
                    let hover = gfx::hovered_item(
                        (x as f64, y as f64),
                        self.center,
                        self.total_slots(),
                        self.cfg.appearance.radius_px as f32,
                    );
                    if hover != self.hover {
                        self.hover = hover;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
            HookEvent::TriggerUp { x: _, y: _ } => {
                if self.held {
                    println!("trigger up, hover = {:?}", self.hover);
                    self.hide_menu_and_act();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Debug aid: show the menu without any hook involvement.
        if std::env::var_os("SIDEQM_AUTOSHOW").is_some() && !self.held {
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
            WindowEvent::RedrawRequested => {
                if let Some(g) = &mut self.gfx {
                    g.render(self.hover);
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

fn apply_no_activate(window: &Window) {
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
    let hwnd = HWND(h.hwnd.get() as *mut _);
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
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

    let mut app = App {
        cfg,
        cfg_raw,
        window: None,
        gfx: None,
        tray: None,
        center: (0.0, 0.0),
        hover: None,
        held: false,
    };
    event_loop.run_app(&mut app).expect("event loop run");
}
