//! Menu geometry: every length or count that both the hit-test (main) and the
//! renderer (gfx) need lives behind `MenuGeometry`, so the drawn Menu and the
//! logical Menu can't drift apart. One-use visual constants stay in gfx::draw.

use std::f32::consts::{FRAC_PI_2, TAU};

use crate::config::Appearance;

/// Snapshot of the Menu's shape for a given config + Item count. Cheap `Copy`;
/// App builds it (startup + config reload) and Gfx keeps a copy — both come
/// from this constructor, so each formula exists only here.
#[derive(Clone, Copy)]
pub struct MenuGeometry {
    scrim_r: f32,
    tile_half: f32,
    hub_ratio: f32,
    label_font_px: f32,
    slot_count: usize,
}

impl MenuGeometry {
    pub fn new(a: &Appearance, item_count: usize) -> MenuGeometry {
        MenuGeometry {
            scrim_r: a.radius_px() as f32,
            tile_half: a.tile_half(),
            hub_ratio: a.hub_ratio(),
            label_font_px: a.label_font_px(),
            // Slots = Items + the meta "Dodaj" slot, always last.
            slot_count: item_count + 1,
        }
    }

    /// Scrim radius — the Menu's backdrop disc.
    pub fn scrim_r(&self) -> f32 {
        self.scrim_r
    }

    pub fn tile_half(&self) -> f32 {
        self.tile_half
    }

    pub fn label_font_px(&self) -> f32 {
        self.label_font_px
    }

    /// Hub radius — also the Dead zone: releasing the Trigger inside cancels.
    pub fn hub_r(&self) -> f32 {
        self.scrim_r * self.hub_ratio
    }

    /// Rest radius: where Tiles sit.
    pub fn rest_r(&self) -> f32 {
        self.scrim_r - self.tile_half - 14.0
    }

    /// Fallback-letter glyph size; draw's positioning ratios are tuned to it.
    pub fn glyph_px(&self) -> f32 {
        self.tile_half * 0.9
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    /// Index of the synthesized "Dodaj" slot (always last).
    pub fn meta_slot(&self) -> usize {
        self.slot_count - 1
    }

    /// Angle of Slot `k`, radians, screen coords (y down).
    /// Slot 0 sits at 12 o'clock; slots proceed clockwise, evenly spaced.
    pub fn slot_angle(&self, k: usize) -> f32 {
        k as f32 * TAU / self.slot_count as f32 - FRAC_PI_2
    }

    /// Gear zone chord: local y-offset from the Menu center (screen y down)
    /// cutting off the bottom 20% of the Hub's height (2 * hub_r).
    pub fn gear_cut_dy(&self) -> f32 {
        self.hub_r() * 0.6
    }

    /// Inside the Gear zone: the Hub's bottom circle segment, below the chord.
    /// Releasing the Trigger here opens config.json; the rest of the Hub stays
    /// the Dead zone.
    pub fn in_gear_zone(&self, cursor: (f64, f64), center: (f64, f64)) -> bool {
        let (dx, dy) = ((cursor.0 - center.0) as f32, (cursor.1 - center.1) as f32);
        let hub_r = self.hub_r();
        dx * dx + dy * dy < hub_r * hub_r && dy > self.gear_cut_dy()
    }

    /// Which Slot the cursor Hovers, if any.
    ///
    /// The whole wedge is the target: there's no outer edge where selection
    /// stops working (overshoot is fine), only the Dead zone (the Hub's own
    /// radius) where releasing cancels instead of launching.
    pub fn hovered_slot(&self, cursor: (f64, f64), center: (f64, f64)) -> Option<usize> {
        let (dx, dy) = ((cursor.0 - center.0) as f32, (cursor.1 - center.1) as f32);
        let hub_r = self.hub_r();
        if dx * dx + dy * dy < hub_r * hub_r {
            return None;
        }
        let angle = dy.atan2(dx);
        (0..self.slot_count).min_by_key(|&k| {
            // shortest angular distance to the slot, scaled to an integer key
            let mut d = (angle - self.slot_angle(k)).rem_euclid(TAU);
            if d > TAU / 2.0 {
                d = TAU - d;
            }
            (d * 10_000.0) as u32
        })
    }

    /// Square window edge length — headroom has to grow with tile/label size
    /// or a large configured tile/label clips at the window edge.
    pub fn window_size(&self) -> u32 {
        let margin = self.tile_half + self.label_font_px * 3.0 + 20.0;
        2 * (self.scrim_r + margin) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 slots, hub_r = 200 * 0.2 = 40 — matches the old free-function tests.
    fn geo(slots: usize) -> MenuGeometry {
        MenuGeometry {
            scrim_r: 200.0,
            tile_half: 32.0,
            hub_ratio: 0.2,
            label_font_px: 13.0,
            slot_count: slots,
        }
    }

    #[test]
    fn dead_zone_cancels() {
        assert_eq!(geo(5).hovered_slot((0.0, 0.0), (0.0, 0.0)), None);
        assert_eq!(geo(5).hovered_slot((10.0, 10.0), (0.0, 0.0)), None);
    }

    #[test]
    fn no_outer_cutoff() {
        // Straight up, way past any tile radius: still slot 0, not None.
        assert_eq!(geo(5).hovered_slot((0.0, -10_000.0), (0.0, 0.0)), Some(0));
    }

    #[test]
    fn slot_zero_is_12_oclock() {
        assert_eq!(geo(5).hovered_slot((0.0, -200.0), (0.0, 0.0)), Some(0));
    }

    #[test]
    fn picks_nearest_sector_clockwise() {
        // 5 slots => 72 degrees apart, so slot 0's wedge is [-126, -54] degrees
        // (0 = +x/3 o'clock, atan2 convention). Well inside stays slot 0;
        // just past the boundary flips to slot 1.
        let center = (0.0, 0.0);
        let inside = angle_point(-90.0 + 30.0, 200.0);
        assert_eq!(geo(5).hovered_slot(inside, center), Some(0));
        let past = angle_point(-90.0 + 40.0, 200.0);
        assert_eq!(geo(5).hovered_slot(past, center), Some(1));
    }

    #[test]
    fn wraps_from_last_slot_to_first() {
        // Just past slot 0's wedge in the other direction (below -126 degrees)
        // wraps to the last slot (4), not -1 or a panic.
        let point = angle_point(-90.0 - 40.0, 200.0);
        assert_eq!(geo(5).hovered_slot(point, (0.0, 0.0)), Some(4));
    }

    #[test]
    fn generic_slot_count() {
        // N != 5 still divides the circle evenly (120 degree slots here).
        assert_eq!(
            geo(3).hovered_slot(angle_point(-90.0, 200.0), (0.0, 0.0)),
            Some(0)
        );
        assert_eq!(
            geo(3).hovered_slot(angle_point(-90.0 + 120.0, 200.0), (0.0, 0.0)),
            Some(1)
        );
    }

    #[test]
    fn dead_zone_is_the_drawn_hub() {
        // The hit-test dead zone and the drawn Hub are the same number now.
        let g = geo(5);
        let just_inside = (0.0, -(g.hub_r() as f64) + 1.0);
        let just_outside = (0.0, -(g.hub_r() as f64) - 1.0);
        assert_eq!(g.hovered_slot(just_inside, (0.0, 0.0)), None);
        assert_eq!(g.hovered_slot(just_outside, (0.0, 0.0)), Some(0));
    }

    #[test]
    fn gear_zone_is_the_hub_bottom_segment() {
        let g = geo(5); // hub_r = 40, cut at +24
        // Bottom of the hub: in the gear zone, and never a slot hover.
        assert!(g.in_gear_zone((0.0, 30.0), (0.0, 0.0)));
        assert_eq!(g.hovered_slot((0.0, 30.0), (0.0, 0.0)), None);
        // Hub center and just above the chord: dead zone, not gear.
        assert!(!g.in_gear_zone((0.0, 0.0), (0.0, 0.0)));
        assert!(!g.in_gear_zone((0.0, 23.0), (0.0, 0.0)));
        // Below the chord line but outside the hub circle: not gear.
        assert!(!g.in_gear_zone((50.0, 30.0), (0.0, 0.0)));
    }

    #[test]
    fn gear_zone_math_is_center_relative() {
        // Same geometry as above but around a real on-screen center: the zone
        // must track the center, not assume the Menu sits at the origin.
        let g = geo(5); // hub_r = 40, cut at +24
        let c = (1000.0, 500.0);
        assert!(g.in_gear_zone((1000.0, 530.0), c)); // 30px below center
        assert!(!g.in_gear_zone((1000.0, 500.0), c)); // at center
        assert!(!g.in_gear_zone((1000.0, 470.0), c)); // above center
        assert_eq!(g.hovered_slot((1000.0, 530.0), c), None); // still dead to launches
    }

    fn angle_point(deg: f32, r: f32) -> (f64, f64) {
        let rad = deg.to_radians();
        (rad.cos() as f64 * r as f64, rad.sin() as f64 * r as f64)
    }
}
