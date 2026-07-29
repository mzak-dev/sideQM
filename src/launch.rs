use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
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

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Registry API in-process, not `reg.exe`: a hidden child process patching the
/// Run key is a persistence pattern AV heuristics (Defender flagged this as
/// Trojan:Win32/Bearfoos.A!ml on another machine) key off, even though it's
/// legitimate here.
///
/// Idempotent: skips the write/delete when the key already matches the
/// desired state, so a normal run (autostart unchanged) never touches the
/// Run key at all. Called unconditionally on every launch and config
/// reload, so without this check every single startup would open the Run
/// key and attempt a write — its own separate persistence-behavior
/// signature (Defender flagged that as Behavior:Win32/Persistence.A!ml).
pub fn set_autostart(enable: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .is_err()
        {
            return;
        }

        let mut buf = [0u16; 520];
        let mut buf_len = (buf.len() * 2) as u32;
        let existing = RegGetValueW(
            key,
            None,
            w!("sideQM"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut buf_len),
        )
        .is_ok()
        .then(|| String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]));

        if enable {
            let Ok(exe) = std::env::current_exe() else {
                let _ = RegCloseKey(key);
                return;
            };
            let exe = exe.to_string_lossy().into_owned();
            if existing.as_deref() != Some(exe.as_str()) {
                let mut value: Vec<u16> = exe.encode_utf16().collect();
                value.push(0);
                let byte_len = (value.len() * 2) as u32;
                let _ = RegSetKeyValueW(
                    key,
                    None,
                    w!("sideQM"),
                    REG_SZ.0,
                    Some(value.as_ptr().cast()),
                    byte_len,
                );
            }
        } else if existing.is_some() {
            let _ = RegDeleteKeyValueW(key, None, w!("sideQM"));
        }
        let _ = RegCloseKey(key);
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
