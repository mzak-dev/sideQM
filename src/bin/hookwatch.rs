//! Diagnostic-only: independent WH_MOUSE_LL observer, logs xbutton events.

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    MSLLHOOKSTRUCT, MSG, WH_MOUSE_LL, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        if msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP {
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            eprintln!("hookwatch: msg={msg:#x} btn={}", (info.mouseData >> 16) & 0xffff);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn main() {
    unsafe {
        SetWindowsHookExW(WH_MOUSE_LL, Some(proc), None, 0).expect("hook");
        eprintln!("hookwatch: installed");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
