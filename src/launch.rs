use std::os::windows::process::CommandExt;
use std::process::Command;

use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, RegSetKeyValueW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, REG_DWORD, RRF_RT_REG_SZ,
};
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

/// Windows 11 demotes new tray icons into the hidden overflow flyout. Flip
/// our own entry's IsPromoted so the icon is actually visible on the taskbar.
/// ponytail: called once after tray creation; on the very first run ever the
/// entry may not exist yet, in which case the next launch fixes it.
pub fn promote_tray_icon() {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.to_string_lossy().to_lowercase();
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Control Panel\NotifyIconSettings"),
            Some(0),
            KEY_READ,
            &mut root,
        )
        .is_err()
        {
            return;
        }
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 64];
            let mut name_len = name.len() as u32;
            if RegEnumKeyExW(
                root,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
            .is_err()
            {
                break;
            }
            index += 1;
            let subkey = PCWSTR(name.as_ptr());
            let mut buf = [0u16; 520];
            let mut buf_len = (buf.len() * 2) as u32;
            if RegGetValueW(
                root,
                subkey,
                w!("ExecutablePath"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut buf_len),
            )
            .is_err()
            {
                continue;
            }
            let path = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
            if path.to_lowercase() == exe {
                let promoted: u32 = 1;
                let _ = RegSetKeyValueW(
                    root,
                    subkey,
                    w!("IsPromoted"),
                    REG_DWORD.0,
                    Some(&promoted as *const u32 as *const _),
                    4,
                );
            }
        }
        let _ = RegCloseKey(root);
    }
}
