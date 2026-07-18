//! Item icon sourcing: explicit PNG path, else the target exe's own icon.

use std::path::{Path, PathBuf};

use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::config::Item;

pub struct RgbaIcon {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub fn icon_for(item: &Item) -> Option<RgbaIcon> {
    if let Some(path) = &item.icon {
        match image::open(path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                return Some(RgbaIcon { pixels: rgba.into_raw(), width, height });
            }
            Err(e) => eprintln!("sideQM: could not load icon {path}: {e}"),
        }
    }
    let path = resolve_target_path(&item.target)?;
    extract_shell_icon(&path)
}

/// Resolve a bare exe name ("wt.exe") through PATH; pass real paths through.
fn resolve_target_path(target: &str) -> Option<PathBuf> {
    if target.contains("://") {
        return None; // URL — nothing to extract from
    }
    let p = Path::new(target);
    if p.exists() {
        return Some(p.to_path_buf());
    }
    if p.components().count() == 1 {
        let name = if p.extension().is_some() {
            target.to_string()
        } else {
            format!("{target}.exe")
        };
        for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
            let candidate = dir.join(&name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

// ponytail: SHGFI_LARGEICON = 32px, slightly soft in a 40px slot; SHGetImageList
// with SHIL_EXTRALARGE (48px) is the upgrade path if it bothers you.
fn extract_shell_icon(path: &Path) -> Option<RgbaIcon> {
    unsafe {
        let mut sfi = SHFILEINFOW::default();
        let res = SHGetFileInfoW(
            &HSTRING::from(path.as_os_str()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if res == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let icon = hicon_to_rgba(sfi.hIcon);
        let _ = DestroyIcon(sfi.hIcon);
        icon
    }
}

unsafe fn hicon_to_rgba(hicon: HICON) -> Option<RgbaIcon> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;

        let mut header = BITMAPINFOHEADER::default();
        header.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        let hdc = GetDC(None);
        // First call fills in dimensions.
        let mut probe = BITMAPINFO { bmiHeader: header, ..Default::default() };
        GetDIBits(hdc, info.hbmColor, 0, 0, None, &mut probe, DIB_RGB_COLORS);
        let width = probe.bmiHeader.biWidth.unsigned_abs();
        let height = probe.bmiHeader.biHeight.unsigned_abs();
        let mut result = None;
        if width > 0 && height > 0 && width <= 512 && height <= 512 {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = width as i32;
            bmi.bmiHeader.biHeight = -(height as i32); // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut pixels = vec![0u8; (width * height * 4) as usize];
            let got = GetDIBits(
                hdc,
                info.hbmColor,
                0,
                height,
                Some(pixels.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            if got != 0 {
                // BGRA -> RGBA
                for px in pixels.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                // Old icons without an alpha channel come back fully transparent.
                if pixels.chunks_exact(4).all(|p| p[3] == 0) {
                    for px in pixels.chunks_exact_mut(4) {
                        px[3] = 255;
                    }
                }
                result = Some(RgbaIcon { pixels, width, height });
            }
        }
        ReleaseDC(None, hdc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        result
    }
}
