use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub trigger_button: TriggerButton,
    pub autostart: bool,
    pub appearance: Appearance,
    pub animation: Animation,
    pub items: Vec<Item>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Animation {
    /// Entrance duration per tile; 0 disables the open animation.
    pub open_ms: u32,
    /// Collective shrink+fade on close; 0 hides instantly.
    pub close_ms: u32,
    /// Per-slot delay during the staggered entrance.
    pub stagger_ms: u32,
    /// 0 = no overshoot, 1 = jelly. Maps to spring damping.
    pub bounciness: f32,
    /// Scale of the selected (Hovered) tile.
    pub hover_scale: f32,
}

impl Default for Animation {
    fn default() -> Self {
        Self { open_ms: 480, close_ms: 180, stagger_ms: 36, bounciness: 0.7, hover_scale: 1.16 }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerButton {
    Mouse4,
    Mouse5,
}

impl TriggerButton {
    /// HIWORD(mouseData) value in WM_XBUTTON* messages: XBUTTON1 = 1, XBUTTON2 = 2.
    pub fn xbutton(self) -> u32 {
        match self {
            TriggerButton::Mouse4 => 1,
            TriggerButton::Mouse5 => 2,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct Appearance {
    pub opacity: f32,
    pub radius_px: u32,
    pub accent_color: String,
    /// Tile edge length in px (tiles are square).
    pub tile_size_px: u32,
    /// Hub radius, and the hit-test dead zone, as a fraction of radius_px.
    pub hub_ratio: f32,
    /// Tile caption font size in px.
    pub label_font_px: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            opacity: 0.95,
            radius_px: 280,
            accent_color: "#5DCAA5".into(),
            tile_size_px: 64,
            hub_ratio: 0.28,
            label_font_px: 13.0,
        }
    }
}

/// Every numeric knob here is user-editable JSON, so every accessor clamps to
/// a range that can't break rendering — garbage in the file degrades to the
/// nearest sane value instead of a broken or crashing menu.
impl Appearance {
    pub fn accent_rgb(&self) -> [f32; 3] {
        parse_hex(&self.accent_color).unwrap_or([0.365, 0.792, 0.647])
    }

    pub fn opacity(&self) -> f32 {
        self.opacity.clamp(0.0, 1.0)
    }

    /// Ring radius in px, clamped so the menu can't collapse to nothing.
    pub fn radius_px(&self) -> u32 {
        self.radius_px.max(80)
    }

    /// Tile half-extent in px — the unit the renderer actually wants.
    pub fn tile_half(&self) -> f32 {
        (self.tile_size_px as f32 / 2.0).clamp(16.0, 120.0)
    }

    /// Hub radius, and the hit-test dead zone, as a fraction of radius_px().
    pub fn hub_ratio(&self) -> f32 {
        self.hub_ratio.clamp(0.05, 0.6)
    }

    pub fn label_font_px(&self) -> f32 {
        self.label_font_px.clamp(8.0, 40.0)
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Item {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger_button: TriggerButton::Mouse5,
            autostart: false,
            appearance: Appearance::default(),
            animation: Animation::default(),
            items: vec![
                Item { name: "Terminal".into(), target: "wt.exe".into(), icon: None },
                Item { name: "Files".into(), target: "explorer.exe".into(), icon: None },
                Item { name: "Browser".into(), target: "https://google.com".into(), icon: None },
                Item { name: "Notepad".into(), target: "notepad.exe".into(), icon: None },
            ],
        }
    }
}

fn parse_hex(s: &str) -> Option<[f32; 3]> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
    ])
}

pub fn config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").expect("APPDATA not set");
    PathBuf::from(appdata).join("sideQM").join("config.json")
}

/// Parse config JSON, tolerating a UTF-8 BOM (Notepad and PowerShell add one).
pub fn parse(raw: &str) -> serde_json::Result<Config> {
    serde_json::from_str(raw.trim_start_matches('\u{feff}'))
}

/// Parse `raw`, then rewrite `path` if the parsed config's canonical form
/// doesn't match what's on disk — options added since the file was last
/// saved, stale formatting, an old field left over from a rename, etc. — so
/// config.json always shows every option the running build understands.
/// Every value the user already set is preserved as-is; only options the
/// file didn't have at all get backfilled, with their defaults.
pub fn parse_and_resync(path: &std::path::Path, raw: &str) -> serde_json::Result<(Config, String)> {
    let cfg = parse(raw)?;
    let canonical = serde_json::to_string_pretty(&cfg).unwrap();
    if raw.trim_start_matches('\u{feff}').trim() == canonical.trim() {
        return Ok((cfg, raw.to_string()));
    }
    if let Err(e) = std::fs::write(path, &canonical) {
        eprintln!("sideQM: could not resync config with current options: {e}");
        return Ok((cfg, raw.to_string()));
    }
    eprintln!("sideQM: config.json updated with current options (existing values kept)");
    Ok((cfg, canonical))
}

/// Returns (config, raw file contents). Creates the file with defaults on first run,
/// and also repopulates it if it exists but is empty (nothing to lose there).
/// A file that fails to parse is NEVER overwritten — we log and fall back to
/// defaults in memory so the user can fix their edits. A file that DOES parse
/// but is missing options added since it was last saved gets resynced (see
/// `parse_and_resync`) so config.json always reflects what the app supports.
pub fn load() -> (Config, String) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(raw) if raw.trim().is_empty() => write_default(&path),
        Ok(raw) => match parse_and_resync(&path, &raw) {
            Ok((cfg, raw)) => (cfg, raw),
            Err(e) => {
                eprintln!("sideQM: config parse error ({e}); using defaults until fixed");
                (Config::default(), raw)
            }
        },
        Err(_) => write_default(&path),
    }
}

fn write_default(path: &std::path::Path) -> (Config, String) {
    let cfg = Config::default();
    let raw = serde_json::to_string_pretty(&cfg).unwrap();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, &raw) {
        eprintln!("sideQM: could not write default config: {e}");
    }
    (cfg, raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_backfill_to_defaults() {
        let cfg = parse("{}").unwrap();
        assert!(cfg == Config::default());
    }

    #[test]
    fn partial_fields_keep_the_rest_at_default() {
        let cfg = parse(r#"{"appearance":{"radius_px":500}}"#).unwrap();
        assert_eq!(cfg.appearance.radius_px, 500);
        assert_eq!(cfg.appearance.accent_color, Appearance::default().accent_color);
        assert!(cfg.animation == Animation::default());
    }

    #[test]
    fn resync_backfills_missing_options_on_disk() {
        let dir = std::env::temp_dir().join(format!("sideqm-test-resync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"appearance":{"radius_px":500}}"#).unwrap();

        let raw_on_disk = std::fs::read_to_string(&path).unwrap();
        let (cfg, raw) = parse_and_resync(&path, &raw_on_disk).unwrap();
        assert_eq!(cfg.appearance.radius_px, 500);
        // the file now advertises every current option, not just the one that was set
        assert!(raw.contains("hub_ratio"));
        assert!(raw.contains("500"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);

        // re-running on the now-canonical text is a no-op: no further rewrite
        let (_, raw2) = parse_and_resync(&path, &raw).unwrap();
        assert_eq!(raw2, raw);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn appearance_accessors_clamp_bad_values() {
        let a = Appearance {
            opacity: 5.0,
            radius_px: 0,
            tile_size_px: 0,
            hub_ratio: 99.0,
            label_font_px: 0.0,
            ..Appearance::default()
        };
        assert!(a.opacity() <= 1.0);
        assert!(a.radius_px() >= 80);
        assert!(a.tile_half() >= 16.0);
        assert!(a.hub_ratio() <= 0.6);
        assert!(a.label_font_px() >= 8.0);
    }
}
