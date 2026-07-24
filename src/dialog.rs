//! Native file-open dialog (Popover browse / icon override) and clipboard
//! text (Ctrl+V in Popover fields). Both blocking, called on the event-loop
//! thread — the hook lives on its own thread, so nothing time-sensitive stalls.

use std::path::PathBuf;

use windows::Win32::Foundation::{HGLOBAL, HWND};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::core::w;

/// Modal file picker owned by our window. Returns None on cancel or any error.
pub fn pick_file(hwnd: HWND, images_only: bool) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        if images_only {
            let spec = [COMDLG_FILTERSPEC {
                pszName: w!("Obrazy PNG"),
                pszSpec: w!("*.png"),
            }];
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
