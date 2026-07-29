//! Icon sourcing. Two sources, in precedence order: an explicit image file
//! (the Item's `icon`), else the target's own shell icon. Everything leaves
//! here as a square RGBA buffer of at most `ICON_BOX` px, so the renderer's
//! square quad never stretches anything.
//!
//! Nothing in this module touches the GPU or the event loop — it runs on the
//! icon worker thread (see `icon_service`).

use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{ImageReader, RgbaImage};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SYSICONINDEX, SHGetFileInfoW, SHGetImageList,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::HSTRING;

/// Target edge for a decoded icon. SHIL_JUMBO is 256, so a jumbo extraction
/// needs no resample at all; anything larger is downscaled to fit.
pub const ICON_BOX: u32 = 256;
/// Refuse absurd source images before the decoder allocates for them — a
/// 4000x4000 PNG is a 64 MB texture nobody asked for.
const MAX_SRC_DIM: u32 = 4096;

/// Image list sizes, from ShlObj_core.h (the windows crate doesn't export them).
const SHIL_EXTRALARGE: i32 = 2;
const SHIL_JUMBO: i32 = 4;

pub struct RgbaIcon {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

// Icons travel inside an AppEvent, which is Debug — printing a few hundred KB
// of pixels helps nobody, so only the shape is shown.
impl std::fmt::Debug for RgbaIcon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RgbaIcon({}x{})", self.width, self.height)
    }
}

/// Where one icon comes from: the Item's two relevant fields, verbatim.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct IconSpec {
    pub icon_path: Option<String>,
    pub target: String,
}

#[derive(Debug)]
pub enum IconError {
    Io(String),
    Decode(String),
    NoIcon,
}

impl std::fmt::Display for IconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IconError::Io(e) => write!(f, "{e}"),
            IconError::Decode(e) => write!(f, "{e}"),
            IconError::NoIcon => write!(f, "no icon could be extracted"),
        }
    }
}

/// The whole pipeline for one spec. Failures are returned, not logged — the
/// caller knows which Item this was for.
pub fn load(spec: &IconSpec) -> Result<RgbaIcon, IconError> {
    if let Some(path) = &spec.icon_path {
        return load_image_file(Path::new(path)).map(normalize);
    }
    let path = resolve_target_path(&spec.target).ok_or(IconError::NoIcon)?;
    extract_shell_icon(&path).ok_or(IconError::NoIcon)
}

/// Decode an image file. Format comes from content sniffing, not the
/// extension, so a mislabeled file still works; SVG is the one format the
/// `image` crate can't sniff, so it dispatches on extension first and on a
/// leading `<?xml`/`<svg` second.
fn load_image_file(path: &Path) -> Result<RgbaImage, IconError> {
    let is_svg_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    if is_svg_ext {
        return rasterize_svg(path, ICON_BOX);
    }
    match decode_raster(path) {
        Ok(img) => Ok(img),
        Err(e) => {
            if looks_like_svg(path) {
                rasterize_svg(path, ICON_BOX)
            } else {
                Err(e)
            }
        }
    }
}

fn looks_like_svg(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let head = &bytes[..bytes.len().min(256)];
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start();
    head.starts_with("<?xml") || head.starts_with("<svg")
}

fn decode_raster(path: &Path) -> Result<RgbaImage, IconError> {
    let reader = ImageReader::open(path)
        .map_err(|e| IconError::Io(e.to_string()))?
        .with_guessed_format()
        .map_err(|e| IconError::Io(e.to_string()))?;
    let mut reader = reader;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SRC_DIM);
    limits.max_image_height = Some(MAX_SRC_DIM);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| IconError::Decode(e.to_string()))?;
    Ok(img.to_rgba8())
}

/// Rasterize at the size we will actually draw, so vector icons are crisp at
/// any tile size instead of being resampled from some arbitrary bitmap.
fn rasterize_svg(path: &Path, box_px: u32) -> Result<RgbaImage, IconError> {
    let data = std::fs::read(path).map_err(|e| IconError::Io(e.to_string()))?;
    rasterize_svg_data(&data, box_px)
}

fn rasterize_svg_data(data: &[u8], box_px: u32) -> Result<RgbaImage, IconError> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &opt).map_err(|e| IconError::Decode(e.to_string()))?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return Err(IconError::Decode("svg has zero size".into()));
    }
    let scale = (box_px as f32 / size.width()).min(box_px as f32 / size.height());
    let w = ((size.width() * scale).round() as u32).clamp(1, box_px);
    let h = ((size.height() * scale).round() as u32).clamp(1, box_px);

    let mut pixmap =
        tiny_skia::Pixmap::new(w, h).ok_or_else(|| IconError::Decode("svg pixmap".into()))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia works premultiplied; the renderer blends straight alpha.
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    RgbaImage::from_raw(w, h, pixels).ok_or_else(|| IconError::Decode("svg buffer".into()))
}

/// Downscale to fit `ICON_BOX`, then pad to a square with transparent margins.
/// Padding on the CPU keeps the renderer's square quad and full-UV sampling
/// honest — a non-square source is letterboxed rather than stretched.
///
/// Never upscales: blowing a 32px shell icon up to 256 on the CPU only bakes
/// in the blur that the GPU's linear sampler would produce anyway.
pub(crate) fn normalize(img: RgbaImage) -> RgbaIcon {
    let (w, h) = img.dimensions();
    let img = if w.max(h) > ICON_BOX {
        let scale = ICON_BOX as f32 / w.max(h) as f32;
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        resize_premultiplied(&img, nw, nh)
    } else {
        img
    };

    let (w, h) = img.dimensions();
    if w == h {
        return RgbaIcon {
            pixels: img.into_raw(),
            width: w,
            height: h,
        };
    }
    let side = w.max(h);
    let mut out = RgbaImage::new(side, side); // zeroed == fully transparent
    image::imageops::replace(
        &mut out,
        &img,
        ((side - w) / 2) as i64,
        ((side - h) / 2) as i64,
    );
    RgbaIcon {
        pixels: out.into_raw(),
        width: side,
        height: side,
    }
}

/// Resampling straight-alpha RGBA bleeds the (usually black) color of fully
/// transparent pixels into the edges, which shows up as a dark halo around
/// every downscaled icon. Premultiplying first is what makes the filter
/// average visible color only.
fn resize_premultiplied(img: &RgbaImage, nw: u32, nh: u32) -> RgbaImage {
    let mut pre = img.clone();
    for px in pre.pixels_mut() {
        let a = px.0[3] as u32;
        for c in 0..3 {
            px.0[c] = ((px.0[c] as u32 * a + 127) / 255) as u8;
        }
    }
    let mut out = image::imageops::resize(&pre, nw, nh, FilterType::CatmullRom);
    for px in out.pixels_mut() {
        let a = px.0[3] as u32;
        if a == 0 {
            px.0 = [0, 0, 0, 0];
            continue;
        }
        for c in 0..3 {
            px.0[c] = (((px.0[c] as u32 * 255) / a).min(255)) as u8;
        }
    }
    out
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

/// The target's own icon, at the largest size the shell will give us.
/// Requires COM on the calling thread (the worker initializes an STA).
fn extract_shell_icon(path: &Path) -> Option<RgbaIcon> {
    let wide = HSTRING::from(path.as_os_str());
    let raw = extract_from_image_list(&wide, SHIL_JUMBO)
        .or_else(|| extract_from_image_list(&wide, SHIL_EXTRALARGE))
        .or_else(|| extract_small_icon(&wide))?;
    // A file with no icon at the requested size comes back as a small glyph
    // stranded in a large transparent canvas; cropping to the visible content
    // is what makes it fill its Tile instead of sitting tiny in the middle.
    Some(normalize(crop_to_content(raw)))
}

fn extract_from_image_list(path: &HSTRING, size: i32) -> Option<RgbaImage> {
    unsafe {
        let mut sfi = SHFILEINFOW::default();
        let res = SHGetFileInfoW(
            path,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if res == 0 {
            return None;
        }
        let list: IImageList = SHGetImageList(size).ok()?;
        let hicon = list.GetIcon(sfi.iIcon, ILD_TRANSPARENT.0 as u32).ok()?;
        let icon = hicon_to_rgba(hicon);
        let _ = DestroyIcon(hicon);
        icon
    }
}

/// Fallback for anything the image lists won't answer for: 32px, no COM.
fn extract_small_icon(path: &HSTRING) -> Option<RgbaImage> {
    unsafe {
        let mut sfi = SHFILEINFOW::default();
        let res = SHGetFileInfoW(
            path,
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

/// Tight bounds of the non-transparent pixels, or None if nothing is visible.
fn alpha_bbox(img: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (x, y, px) in img.enumerate_pixels() {
        if px.0[3] != 0 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    (x0 != u32::MAX).then(|| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

fn crop_to_content(img: RgbaImage) -> RgbaImage {
    let Some((x, y, w, h)) = alpha_bbox(&img) else {
        return img; // fully transparent: nothing to crop toward
    };
    if (w, h) == img.dimensions() {
        return img;
    }
    image::imageops::crop_imm(&img, x, y, w, h).to_image()
}

unsafe fn hicon_to_rgba(hicon: HICON) -> Option<RgbaImage> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;

        let mut header = BITMAPINFOHEADER::default();
        header.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        let hdc = GetDC(None);
        // First call fills in dimensions.
        let mut probe = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
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
                result = RgbaImage::from_raw(width, height, pixels);
            }
        }
        ReleaseDC(None, hdc);
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        result
    }
}

// --- Icon Library -------------------------------------------------------
// Icons picked in the Popover are copied here at commit time, and the config
// points at the copy. Without that, moving or deleting the original silently
// breaks the Item's icon long after the user has forgotten where it came from.
// This is a library, not a cache: deleting it loses data.

pub fn icons_dir() -> PathBuf {
    let cfg = crate::config::config_path();
    let dir = cfg.parent().unwrap_or_else(|| Path::new("."));
    dir.join("icons")
}

/// Copy `src` into the library, named by content hash so re-adding the same
/// image reuses one file. Returns the library path.
pub fn import_to_library(src: &Path) -> std::io::Result<PathBuf> {
    import_into(&icons_dir(), src)
}

fn import_into(dir: &Path, src: &Path) -> std::io::Result<PathBuf> {
    if src.parent() == Some(dir) {
        return Ok(src.to_path_buf()); // already ours
    }
    let bytes = std::fs::read(src)?;
    let ext: String = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("img")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let ext = if ext.is_empty() { "img".into() } else { ext };
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(format!("{:016x}.{}", fnv1a64(&bytes), ext.to_ascii_lowercase()));
    if !dest.exists() {
        std::fs::write(&dest, &bytes)?;
    }
    Ok(dest)
}

/// FNV-1a. Not cryptographic — it only has to be stable across versions and
/// spread well enough that two different icons don't collide, which rules out
/// DefaultHasher (explicitly unstable across releases).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sideqm-test-icons-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn solid(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([10, 200, 120, 255]))
    }

    #[test]
    fn normalize_letterboxes_instead_of_stretching() {
        let icon = normalize(solid(100, 50));
        assert_eq!((icon.width, icon.height), (100, 100));
        // Padding is transparent, content is centered: row 0 is padding, row 50 is content.
        let alpha_at = |x: u32, y: u32| icon.pixels[((y * icon.width + x) * 4 + 3) as usize];
        assert_eq!(alpha_at(50, 0), 0);
        assert_eq!(alpha_at(50, 50), 255);
        assert_eq!(alpha_at(50, 99), 0);

        // Tall source: the padding moves to the left and right edges.
        let icon = normalize(solid(50, 100));
        assert_eq!((icon.width, icon.height), (100, 100));
        let alpha_at = |x: u32, y: u32| icon.pixels[((y * icon.width + x) * 4 + 3) as usize];
        assert_eq!(alpha_at(0, 50), 0);
        assert_eq!(alpha_at(50, 50), 255);
        assert_eq!(alpha_at(99, 50), 0);
    }

    #[test]
    fn normalize_downscales_but_never_upscales() {
        let icon = normalize(solid(512, 512));
        assert_eq!((icon.width, icon.height), (ICON_BOX, ICON_BOX));

        let icon = normalize(solid(32, 32));
        assert_eq!((icon.width, icon.height), (32, 32));
    }

    #[test]
    fn resize_does_not_bleed_transparent_black_into_edges() {
        // Opaque white left half, fully transparent (black) right half. A naive
        // straight-alpha resize darkens the seam; premultiplied does not.
        let mut img = RgbaImage::new(64, 8);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = if x < 32 {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([0, 0, 0, 0])
            };
        }
        let out = resize_premultiplied(&img, 32, 4);
        // Every pixel with meaningful alpha must still be white, not grey.
        for px in out.pixels() {
            if px.0[3] > 128 {
                assert!(px.0[0] > 240, "halo: {:?}", px.0);
            }
        }
    }

    #[test]
    fn decodes_every_supported_raster_format() {
        let dir = temp_dir("formats");
        let rgba = image::DynamicImage::ImageRgba8(solid(8, 8));
        for (fmt, ext) in [
            (ImageFormat::Png, "png"),
            (ImageFormat::Jpeg, "jpg"),
            (ImageFormat::Bmp, "bmp"),
            (ImageFormat::Gif, "gif"),
            (ImageFormat::Ico, "ico"),
        ] {
            let path = dir.join(format!("icon.{ext}"));
            // JPEG has no alpha channel to encode into.
            let source = if fmt == ImageFormat::Jpeg {
                image::DynamicImage::ImageRgb8(rgba.to_rgb8())
            } else {
                rgba.clone()
            };
            source
                .save_with_format(&path, fmt)
                .unwrap_or_else(|e| panic!("encode {ext}: {e}"));
            let decoded = load_image_file(&path).unwrap_or_else(|e| panic!("decode {ext}: {e}"));
            assert_eq!(decoded.dimensions(), (8, 8), "{ext}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// WebP is decode-only in this build, so the fixture is a real file rather
    /// than something encoded here: a 1x1 lossless VP8L frame.
    #[test]
    fn decodes_webp() {
        let dir = temp_dir("webp");
        let path = dir.join("icon.webp");
        let webp: &[u8] = &[
            0x52, 0x49, 0x46, 0x46, 0x1a, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x4c, 0x0d, 0x00, 0x00, 0x00, 0x2f, 0x00, 0x00, 0x00, 0x10, 0x07, 0x10, 0x11,
            0x11, 0x88, 0x88, 0xfe, 0x07, 0x00,
        ];
        std::fs::write(&path, webp).unwrap();
        let decoded = load_image_file(&path).expect("webp decode");
        assert_eq!(decoded.dimensions(), (1, 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sniffs_content_rather_than_trusting_the_extension() {
        let dir = temp_dir("sniff");
        let path = dir.join("actually-a-png.jpg");
        solid(8, 8).save_with_format(&path, ImageFormat::Png).unwrap();
        assert!(load_image_file(&path).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_images_larger_than_the_source_limit() {
        let dir = temp_dir("limits");
        let path = dir.join("huge.png");
        // Tiny on disk (one flat color), way over the limit in pixels.
        RgbaImage::from_pixel(MAX_SRC_DIM + 1, 4, Rgba([1, 2, 3, 255]))
            .save_with_format(&path, ImageFormat::Png)
            .unwrap();
        assert!(matches!(
            decode_raster(&path),
            Err(IconError::Decode(_)) | Err(IconError::Io(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rasterizes_svg_at_the_target_box_and_survives_garbage() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#0f0"/></svg>"##;
        let img = rasterize_svg_data(svg, ICON_BOX).unwrap();
        assert_eq!(img.dimensions(), (ICON_BOX, ICON_BOX));
        assert_eq!(img.get_pixel(128, 128).0[3], 255);

        assert!(rasterize_svg_data(b"<svg not really", ICON_BOX).is_err());
    }

    #[test]
    fn svg_dispatch_covers_a_mislabeled_file() {
        let dir = temp_dir("svgext");
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#00f"/></svg>"##;
        // Right extension.
        let path = dir.join("a.SVG");
        std::fs::write(&path, svg).unwrap();
        assert!(load_image_file(&path).is_ok());
        // Wrong extension: raster decode fails, the `<svg` sniff rescues it.
        let path = dir.join("b.png");
        std::fs::write(&path, svg).unwrap();
        assert!(load_image_file(&path).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn alpha_bbox_finds_content_and_reports_empty() {
        // 32x32 of content stranded in the corner of a 256x256 canvas — what a
        // jumbo extraction returns for a file that has no jumbo-sized icon.
        let mut img = RgbaImage::new(256, 256);
        for y in 0..32 {
            for x in 0..32 {
                img.put_pixel(x, y, Rgba([9, 9, 9, 255]));
            }
        }
        assert_eq!(alpha_bbox(&img), Some((0, 0, 32, 32)));
        assert_eq!(crop_to_content(img).dimensions(), (32, 32));

        assert_eq!(alpha_bbox(&RgbaImage::new(16, 16)), None);
    }

    #[test]
    fn library_dedupes_identical_content_and_separates_distinct() {
        let dir = temp_dir("library");
        let lib = dir.join("icons");

        let a = dir.join("a.png");
        let b = dir.join("b.png");
        let c = dir.join("c.png");
        std::fs::write(&a, b"same-bytes").unwrap();
        std::fs::write(&b, b"same-bytes").unwrap();
        std::fs::write(&c, b"other-bytes").unwrap();

        let pa = import_into(&lib, &a).unwrap();
        let pb = import_into(&lib, &b).unwrap();
        let pc = import_into(&lib, &c).unwrap();
        assert_eq!(pa, pb, "identical content must reuse one library file");
        assert_ne!(pa, pc);
        assert!(pa.starts_with(&lib));
        assert_eq!(std::fs::read(&pa).unwrap(), b"same-bytes");
        assert_eq!(std::fs::read_dir(&lib).unwrap().count(), 2);

        // Importing something already in the library is a no-op passthrough.
        assert_eq!(import_into(&lib, &pa).unwrap(), pa);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fnv1a64_matches_the_reference_vector() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }
}
