//! Append log next to config.json. The release build is `windows_subsystem =
//! "windows"` — there is no console, so an `eprintln!` about a broken icon path
//! reaches nobody and the failure reads as "the app ignored my setting". Every
//! user-facing failure goes here instead.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

static LOG: Mutex<Option<File>> = Mutex::new(None);

pub fn path() -> PathBuf {
    let cfg = crate::config::config_path();
    let dir = cfg.parent().unwrap_or_else(|| std::path::Path::new("."));
    dir.join("sideqm.log")
}

/// Truncate-on-start is the whole size policy: one run's worth of history is
/// what anyone ever reads, and nothing has to rotate or prune.
pub fn init() {
    init_at(&path());
}

fn init_at(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = File::create(path).ok();
    *LOG.lock().unwrap_or_else(|e| e.into_inner()) = file;
}

/// Never panics and never propagates: a failure to log must not take down the
/// app that was trying to report a failure.
pub fn log(msg: &str) {
    #[cfg(debug_assertions)]
    eprintln!("sideQM: {msg}");

    let stamp = unsafe { GetLocalTime() };
    let mut guard = LOG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(
            file,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}  {msg}",
            stamp.wYear, stamp.wMonth, stamp.wDay, stamp.wHour, stamp.wMinute, stamp.wSecond
        );
        let _ = file.flush();
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::logging::log(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_lines_and_truncates_on_init() {
        let dir = std::env::temp_dir().join(format!("sideqm-test-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sideqm.log");

        init_at(&path);
        log("marker-alpha");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("marker-alpha"));
        // Timestamped, not a bare message.
        assert!(text.trim_start().starts_with("20"));

        // Re-init starts a fresh file rather than growing forever.
        init_at(&path);
        log("marker-beta");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("marker-alpha"));
        assert!(text.contains("marker-beta"));

        // Leave the global unset so other tests don't write into a temp dir
        // that is about to disappear.
        *LOG.lock().unwrap_or_else(|e| e.into_inner()) = None;
        std::fs::remove_dir_all(&dir).ok();
    }
}
