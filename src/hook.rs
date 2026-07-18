//! Low-level global mouse hook on a DEDICATED thread whose only job is a
//! message pump. It must not share the winit/render thread: Windows enforces a
//! response deadline on WH_MOUSE_LL (LowLevelHooksTimeout, ~300ms) and
//! silently disconnects hooks whose thread stalls past it — which the first
//! wgpu present to a freshly shown window reliably does. (Diagnosed
//! empirically: hook died after the first visible frame when installed on the
//! winit thread.)

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    MSLLHOOKSTRUCT, MSG, WH_MOUSE_LL, WM_MOUSEMOVE, WM_XBUTTONDOWN, WM_XBUTTONUP,
};
use winit::event_loop::EventLoopProxy;

use crate::AppEvent;

#[derive(Debug)]
pub enum HookEvent {
    TriggerDown { x: i32, y: i32 },
    TriggerUp { x: i32, y: i32 },
    Move { x: i32, y: i32 },
}

static PROXY: OnceLock<EventLoopProxy<AppEvent>> = OnceLock::new();
/// XBUTTON id (1 = MB4/back, 2 = MB5/forward) the hook swallows. Updated on config reload.
pub static TRIGGER_XBUTTON: AtomicU32 = AtomicU32::new(2);
/// Set while the menu is visible so the hook only forwards mouse moves we care about.
pub static MENU_OPEN: AtomicBool = AtomicBool::new(false);

pub fn install(proxy: EventLoopProxy<AppEvent>) {
    PROXY.set(proxy).expect("hook installed twice");
    std::thread::spawn(|| {
        unsafe {
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)
                .expect("failed to install WH_MOUSE_LL hook");
            // Plain pump forever; hook callbacks are dispatched from GetMessageW.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        match msg {
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                let xbutton = (info.mouseData >> 16) & 0xffff;
                if xbutton == TRIGGER_XBUTTON.load(Ordering::Relaxed) {
                    let (x, y) = (info.pt.x, info.pt.y);
                    let ev = if msg == WM_XBUTTONDOWN {
                        HookEvent::TriggerDown { x, y }
                    } else {
                        HookEvent::TriggerUp { x, y }
                    };
                    if let Some(proxy) = PROXY.get() {
                        let _ = proxy.send_event(AppEvent::Hook(ev));
                    }
                    return LRESULT(1); // swallow: apps never see this button
                }
            }
            WM_MOUSEMOVE => {
                if MENU_OPEN.load(Ordering::Relaxed) {
                    if let Some(proxy) = PROXY.get() {
                        let _ = proxy.send_event(AppEvent::Hook(HookEvent::Move {
                            x: info.pt.x,
                            y: info.pt.y,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
