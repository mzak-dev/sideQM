use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Open anything the shell can: exe, URL, folder, document.
pub fn open(target: &str) {
    let target = HSTRING::from(target);
    unsafe {
        ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// ponytail: reg.exe instead of registry API code — one process call, same result.
pub fn set_autostart(enable: bool) {
    let status = if enable {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return,
        };
        Command::new("reg")
            .args(["add", RUN_KEY, "/v", "sideQM", "/t", "REG_SZ", "/d"])
            .arg(&exe)
            .args(["/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    } else {
        // Deleting a key that doesn't exist fails; that's fine, ignore it quietly.
        Command::new("reg")
            .args(["delete", RUN_KEY, "/v", "sideQM", "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    };
    if enable {
        if let Err(e) = status {
            eprintln!("sideQM: autostart registration failed: {e}");
        }
    }
}
