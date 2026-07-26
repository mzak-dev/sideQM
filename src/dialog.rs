//! Native file-open dialog (Popover browse / icon override) and clipboard
//! text (Ctrl+V in Popover fields).
//!
//! The picker runs on its own thread. Running it inline used to eat the
//! Popover: winit buffers events raised while a handler is executing, so the
//! `Focused(false)` the dialog caused was delivered *after* the handler
//! returned and the "a dialog is up" guard had already been cleared — the
//! Popover was discarded and the item lost. See ADR-0004.

use std::path::PathBuf;

use windows::Win32::Foundation::{HGLOBAL, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::core::w;
use winit::event_loop::EventLoopProxy;

use crate::AppEvent;

/// Which Popover field a pick is destined for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickPurpose {
    Target,
    Icon,
}

/// Show the picker on a scratch thread and post the result back as an
/// `AppEvent::FilePicked`. `hwnd_raw` rather than `HWND` because a raw window
/// handle isn't `Send`; the dialog only needs it to pick an owner to be modal
/// against, which works across threads.
pub fn pick_file_async(hwnd_raw: isize, purpose: PickPurpose, proxy: EventLoopProxy<AppEvent>) {
    let worker_proxy = proxy.clone();
    let spawned = std::thread::Builder::new()
        .name("sideqm-file-dialog".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let path = pick_file(HWND(hwnd_raw as *mut _), purpose == PickPurpose::Icon);
            unsafe { CoUninitialize() };
            let _ = worker_proxy.send_event(AppEvent::FilePicked { purpose, path });
        });
    if let Err(e) = spawned {
        // Nothing opened, so nothing will report back: release the guard.
        crate::log!("could not open the file picker: {e}");
        let _ = proxy.send_event(AppEvent::FilePicked {
            purpose,
            path: None,
        });
    }
}

/// Modal file picker owned by our window. Returns None on cancel or any error.
fn pick_file(hwnd: HWND, images_only: bool) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        if images_only {
            let spec = [
                COMDLG_FILTERSPEC {
                    pszName: w!("Obrazy"),
                    pszSpec: w!("*.png;*.jpg;*.jpeg;*.webp;*.ico;*.bmp;*.gif;*.svg"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("Wszystkie pliki"),
                    pszSpec: w!("*.*"),
                },
            ];
            dlg.SetFileTypes(&spec).ok()?;
        }
        dlg.Show(Some(hwnd)).ok()?; // Err on cancel
        let item = dlg.GetResult().ok()?;
        let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = pw.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(pw.as_ptr() as *const _));
        path
    }
}

const CF_UNICODETEXT: u32 = 13;

/// Clipboard text, single-lined (newlines become spaces) — these are one-line fields.
pub fn clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT).ok().and_then(|handle| {
            let hglobal = HGLOBAL(handle.0);
            let ptr = GlobalLock(hglobal) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(hglobal);
            Some(s)
        });
        let _ = CloseClipboard();
        text.map(|s| s.replace(['\r', '\n'], " ").trim().to_string())
            .filter(|s| !s.is_empty())
    }
}
