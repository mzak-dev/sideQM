//! Presentation: a CPU-rasterized pixmap handed to DWM through a layered window.
//!
//! ADR-0007: nothing here touches the GPU. `UpdateLayeredWindow` takes a
//! premultiplied BGRA bitmap and DWM composites it, so there is no swapchain to
//! lose, to resize, or to overdraw — the three ways the wgpu presentation path
//! used to take the display driver down with it (ADR-0001, ADR-0002).

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC,
    GetObjectW, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_EXSTYLE, GWL_STYLE, GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, ULW_ALPHA,
    UpdateLayeredWindow, WS_EX_LAYERED,
};

/// Put `WS_EX_LAYERED` back if it has gone missing.
///
/// winit's Windows backend keeps its own model of the window flags and rewrites
/// the whole ex-style from it whenever anything changes it (`set_visible`, the
/// window level, the taskbar flag). That wipes bits set behind its back —
/// including this one, without which every `UpdateLayeredWindow` fails with
/// ERROR_INVALID_PARAMETER. Checking per frame is cheaper than auditing every
/// winit call that might have clobbered it, and it heals itself if a new one
/// appears.
fn ensure_layered(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & WS_EX_LAYERED.0 as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
        }
    }
}

/// A top-down 32-bit DIB section and the memory DC it is selected into: the
/// staging buffer `UpdateLayeredWindow` reads one frame from.
pub struct Layered {
    hwnd: HWND,
    /// Screen DC, used for palette matching on the destination side.
    screen: HDC,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    /// The DIB's pixels, owned by GDI. Top-down BGRA, premultiplied.
    bits: *mut u8,
    width: u32,
    height: u32,
    /// A failing present repeats every frame; the details are only worth
    /// printing the first time.
    diagnosed: bool,
}

impl Layered {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Option<Layered> {
        let (width, height) = (width.max(1), height.max(1));
        ensure_layered(hwnd);
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width as i32;
        // Negative height means top-down, which is the row order a tiny-skia
        // Pixmap already has — the alternative is flipping every frame.
        info.bmiHeader.biHeight = -(height as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;

        unsafe {
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                return None;
            }
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                let _ = DeleteObject(bitmap.into());
                return None;
            }
            let previous = SelectObject(dc, bitmap.into());
            Some(Layered {
                hwnd,
                screen: GetDC(None),
                dc,
                bitmap,
                previous,
                bits: bits.cast(),
                width,
                height,
                diagnosed: false,
            })
        }
    }

    /// Everything that can make `UpdateLayeredWindow` reject a call, printed
    /// once so a bad frame names its own cause instead of repeating an error
    /// code sixty times a second.
    fn diagnose(&self) {
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let style = GetWindowLongPtrW(self.hwnd, GWL_STYLE);
            let mut rect = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut rect);
            let mut bm = BITMAP::default();
            let got = GetObjectW(
                self.bitmap.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut bm as *mut BITMAP).cast()),
            );
            eprintln!(
                "sideQM: ULW rejected: ex={ex:#x} style={style:#x} \
                 window={}x{} dib={}x{}@{}bpp(GetObject={got}) surface={}x{} \
                 screen_dc_null={} mem_dc_null={}",
                rect.right - rect.left,
                rect.bottom - rect.top,
                bm.bmWidth,
                bm.bmHeight,
                bm.bmBitsPixel,
                self.width,
                self.height,
                self.screen.is_invalid(),
                self.dc.is_invalid(),
            );
        }
    }

    /// Hand one frame to DWM. `rgba` is premultiplied RGBA, top-down — exactly
    /// what `Pixmap::data()` returns.
    pub fn present(&mut self, rgba: &[u8]) {
        let len = (self.width as usize) * (self.height as usize) * 4;
        if rgba.len() < len {
            return;
        }
        ensure_layered(self.hwnd);
        // GDI wants BGRA. Both sides are premultiplied already, so swapping the
        // red and blue channels is the entire conversion.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, len) };
        for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }

        let size = SIZE {
            cx: self.width as i32,
            cy: self.height as i32,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // A null destination point leaves the window where it is; moving it
        // stays winit's job.
        unsafe {
            if let Err(e) = UpdateLayeredWindow(
                self.hwnd,
                Some(self.screen),
                None,
                Some(&size),
                Some(self.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            ) {
                if !self.diagnosed {
                    self.diagnosed = true;
                    eprintln!("sideQM: UpdateLayeredWindow failed: {e}");
                    self.diagnose();
                }
            }
        }
    }
}

impl Drop for Layered {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
            ReleaseDC(None, self.screen);
        }
    }
}

/// Block until DWM's next composition pass. This is the whole frame-pacing
/// story: without it the redraw loop spins as fast as the CPU can rasterize,
/// and with it frames land on the compositor's own rhythm.
pub fn wait_for_vblank() {
    unsafe {
        let _ = DwmFlush();
    }
}
