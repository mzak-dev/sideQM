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
    /// Max Bulge scale of the tile under the cursor.
    pub hover_scale: f32,
}

impl Default for Animation {
    fn default() -> Self {
        Self { open_ms: 250, close_ms: 140, stagger_ms: 25, bounciness: 0.4, hover_scale: 1.4 }
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
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            opacity: 0.45,
            radius_px: 280,
            accent_color: "#2ecc71".into(),
        }
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

impl Appearance {
    pub fn accent_rgb(&self) -> [f32; 3] {
        parse_hex(&self.accent_color).unwrap_or([0.18, 0.8, 0.44])
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

/// Returns (config, raw file contents). Creates the file with defaults on first run,
/// and also repopulates it if it exists but is empty (nothing to lose there).
/// A file with actual content that fails to parse is NEVER overwritten — we log and
/// fall back to defaults in memory so the user can fix their edits.
pub fn load() -> (Config, String) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(raw) if raw.trim().is_empty() => write_default(&path),
        Ok(raw) => match parse(&raw) {
            Ok(cfg) => (cfg, raw),
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
