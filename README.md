# sideQM
## Disclaimer:
## It is fully Prompt Engineered (in simple words Vibe Coded) using Claude Code i always wanted app like that and i wanted to experiment with rust and ai, don't treat it like it is fully stable/ready

A translucent radial launcher for Windows. Hold a mouse side button anywhere
on the desktop, sweep toward an app, release to launch it.

[![Rust CI](https://github.com/mzak-dev/sideQM/actions/workflows/rust.yml/badge.svg)](https://github.com/mzak-dev/sideQM/actions/workflows/rust.yml)

<img width="480" height="480" alt="output" src="https://github.com/user-attachments/assets/e3e8517c-4b69-473b-865c-2f959c902567" />

## How it works

Holding the **Trigger** (a mouse side button) summons the **Menu**: a
translucent ring of tiles centered on the cursor. Moving the cursor **Hovers**
one tile at a time — the center **Hub** previews its name, and a mint **Arc**
points at it. Release over a tile to launch it; release over the center
(the dead zone) to dismiss without launching anything.

## Features

- Configurable **Trigger** button (`Mouse4`/`Mouse5`); launch targets can be
  exes, URLs, folders, or documents — anything the shell can open
- Spring-animated open/close with per-tile stagger, tuned entirely via config
- **Dodaj** slot adds a new item inline — name, target (with a file browse
  dialog), optional icon — no manual JSON editing required
- Gear zone on the Hub opens `config.json` directly for advanced tweaks
- Lives in the system tray; optional autostart with Windows
- `config.json` is auto-repopulated with any options added since it was last
  saved, keeping every value you already set

## Install

Windows only. Grab `sideqm.exe` from
[Releases](https://github.com/mzak-dev/sideQM/releases), or build from source:

```bash
cargo build --release
# binary at target/release/sideqm.exe
```

Requires Rust (edition 2024).

## Configuration

Settings live at `%APPDATA%\sideQM\config.json` and are created with sane
defaults on first run:

```json
{
  "trigger_button": "mouse5",
  "autostart": false,
  "appearance": {
    "opacity": 0.95,
    "radius_px": 280,
    "accent_color": "#5DCAA5",
    "tile_size_px": 64,
    "hub_ratio": 0.28,
    "label_font_px": 13.0
  },
  "animation": {
    "open_ms": 480,
    "close_ms": 180,
    "stagger_ms": 36,
    "bounciness": 0.7,
    "hover_scale": 1.16
  },
  "items": [
    { "name": "Terminal", "target": "wt.exe" },
    { "name": "Files", "target": "explorer.exe" }
  ]
}
```

| Field                    | Meaning                                                   |
| ------------------------ | ---------------------------------------------------------- |
| `trigger_button`         | `mouse4` or `mouse5` — which side button summons the Menu |
| `appearance.radius_px`   | Rest radius of the tile ring                              |
| `appearance.accent_color`| Hex color for the Arc and Hub accent                       |
| `animation.bounciness`   | `0` = no overshoot, `1` = jelly                            |
| `animation.hover_scale`  | Scale applied to the Hovered tile                          |
| `items[].target`         | Executable, URL, folder, or document to launch             |
| `items[].icon`           | Optional PNG path; otherwise the target's own shell icon is used (not available for URLs) |

## Development

```bash
cargo build
cargo test
```

CI builds and tests on `windows-latest` for every PR to `master`
([rust.yml](.github/workflows/rust.yml)). Merging to `master` re-runs the tests
in release mode and, if the `version` in `Cargo.toml` has no matching GitHub
release yet, publishes one with the built binary
([release.yml](.github/workflows/release.yml)). So cutting a release is a
version bump in `Cargo.toml` — no tagging by hand.

- [`CONTEXT.md`](CONTEXT.md) — domain glossary (Menu, Trigger, Slot, Hover, Arc, ...)
- [`docs/adr/`](docs/adr) — architecture decisions (e.g. why rendering is pinned to DX12)

Debug env vars: `SIDEQM_CONFIG_PATH` (point at a scratch config file),
`SIDEQM_BACKEND` (`vulkan`/`gl` override), `SIDEQM_AUTOSHOW` (open the Menu
without holding the Trigger).
