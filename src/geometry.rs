//! Menu geometry: every length or count that both the hit-test (main) and the
//! renderer (gfx) need lives behind `MenuGeometry`, so the drawn Menu and the
//! logical Menu can't drift apart. One-use visual constants stay in gfx::draw.

use std::f32::consts::{FRAC_PI_2, TAU};

use crate::config::{AddPosition, Config};
use crate::popover;

/// Smallest Hub radius the Dodaj slot's toggle (switch + label) fits inside.
/// Only ever applied while Pinned — see `hub_r_pinned`.
const PINNED_HUB_MIN_R: f32 = 44.0;

/// Snapshot of the Menu's shape for a given config + Item count. Cheap `Copy`;
/// App builds it (startup + config reload) and Gfx keeps a copy — both come
/// from this constructor, so each formula exists only here.
#[derive(Clone, Copy)]
pub struct MenuGeometry {
    scrim_r: f32,
    tile_half: f32,
    hub_ratio: f32,
    label_font_px: f32,
    item_count: usize,
    /// Where the meta "Dodaj" slot sits, or None when it is hidden.
    add_at: Option<AddPosition>,
}

impl MenuGeometry {
    pub fn new(cfg: &Config) -> MenuGeometry {
        let a = &cfg.appearance;
        MenuGeometry {
            scrim_r: a.radius_px() as f32,
            tile_half: a.tile_half(),
            hub_ratio: a.hub_ratio(),
            label_font_px: a.label_font_px(),
            item_count: cfg.items.len(),
            add_at: (!cfg.add_slot.hidden).then_some(cfg.add_slot.position),
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

    /// Hub radius — also the Dead zone: releasing the Trigger inside cancels,
    /// and the Gear zone is carved out of it. One number for all three, which
    /// is what keeps the drawn Hub and the hit-test from drifting apart.
    pub fn hub_r(&self) -> f32 {
        self.scrim_r * self.hub_ratio
    }

    /// Hub radius while Pinned, floored so the Dodaj slot's toggle always fits.
    ///
    /// Deliberately NOT part of `hub_r`: that one is also the Dead zone and the
    /// Gear zone, and raising it would move where the Trigger has to be released
    /// to cancel. Neither of those is in play while Pinned — the Trigger isn't
    /// held — so the floor is safe here and nowhere else.
    pub fn hub_r_pinned(&self) -> f32 {
        self.hub_r().max(PINNED_HUB_MIN_R)
    }

    /// Rest radius: where Tiles sit.
    pub fn rest_r(&self) -> f32 {
        self.scrim_r - self.tile_half - 14.0
    }

    /// Fallback-letter glyph size; draw's positioning ratios are tuned to it.
    pub fn glyph_px(&self) -> f32 {
        self.tile_half * 0.9
    }

    pub fn item_count(&self) -> usize {
        self.item_count
    }

    /// Slots = Items, plus the meta "Dodaj" slot unless it is hidden. Zero is a
    /// legal count: no Items and a hidden Dodaj slot draws an empty Menu, and
    /// the Gear zone still works, so Pinned stays reachable.
    pub fn slot_count(&self) -> usize {
        self.item_count + self.add_at.is_some() as usize
    }

    /// Index of the synthesized "Dodaj" slot, or None when it is hidden.
    pub fn meta_slot(&self) -> Option<usize> {
        match self.add_at? {
            AddPosition::First => Some(0),
            AddPosition::Last => Some(self.slot_count() - 1),
        }
    }

    /// The Item a Slot shows, or None for the meta slot. Slot index and Item
    /// index only coincide when the Dodaj slot isn't sitting first.
    pub fn item_at(&self, slot: usize) -> Option<usize> {
        if self.meta_slot() == Some(slot) || slot >= self.slot_count() {
            return None;
        }
        Some(match self.add_at {
            Some(AddPosition::First) => slot - 1,
            _ => slot,
        })
    }

    /// The Slot showing Item `i` — the inverse of `item_at`.
    pub fn slot_of_item(&self, i: usize) -> usize {
        match self.add_at {
            Some(AddPosition::First) => i + 1,
            _ => i,
        }
    }

    /// Where a dragged Tile lands if dropped on `slot`. The meta slot is not a
    /// drop target, so it clamps to the nearest Item index instead of being
    /// rejected — dropping "past the end" means last, not nothing.
    pub fn drop_index(&self, slot: usize) -> usize {
        let last = self.item_count.saturating_sub(1);
        self.item_at(slot).unwrap_or(match self.add_at {
            Some(AddPosition::First) => 0,
            _ => last,
        })
        .min(last)
    }

    /// Angle of Slot `k`, radians, screen coords (y down).
    /// Slot 0 sits at 12 o'clock; slots proceed clockwise, evenly spaced.
    pub fn slot_angle(&self, k: usize) -> f32 {
        // An empty Menu has no slots to place; the divisor must never be zero,
        // or the NaN rides straight into the vertex buffer.
        let n = self.slot_count().max(1);
        k as f32 * TAU / n as f32 - FRAC_PI_2
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

    /// Which Transport button (see Now Playing) sits under the cursor, if any.
    /// Occupies the Hub minus the Gear zone's chord — everything `in_gear_zone`
    /// excludes — split into three equal-width vertical strips, left to right.
    /// Fires on a left click, never on releasing the Trigger, so it does not
    /// have to dodge the Dead zone the way `hovered_slot` does.
    pub fn transport_button(&self, p: (f32, f32)) -> Option<TransportButton> {
        let hub_r = self.hub_r();
        if p.0 * p.0 + p.1 * p.1 >= hub_r * hub_r
            || p.1 > self.gear_cut_dy()
            || self.on_title_arc(p)
        {
            return None;
        }
        let third = hub_r / 3.0;
        Some(if p.0 < -third {
            TransportButton::Prev
        } else if p.0 > third {
            TransportButton::Next
        } else {
            TransportButton::PlayPause
        })
    }

    /// The Title arc's hover region — a band hugging the top inside edge of
    /// the Hub, where the curved title is actually drawn. Hovering it reveals
    /// the artist; it never overlaps a Transport button (it sits above them).
    pub fn on_title_arc(&self, p: (f32, f32)) -> bool {
        let hub_r = self.hub_r();
        let d2 = p.0 * p.0 + p.1 * p.1;
        let outer = hub_r * 0.96;
        let inner = hub_r * 0.62;
        d2 <= outer * outer && d2 >= inner * inner && p.1 < -hub_r * 0.3
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
        (0..self.slot_count()).min_by_key(|&k| {
            // shortest angular distance to the slot, scaled to an integer key
            let mut d = (angle - self.slot_angle(k)).rem_euclid(TAU);
            if d > TAU / 2.0 {
                d = TAU - d;
            }
            (d * 10_000.0) as u32
        })
    }

    /// Center of Slot `k`'s Tile, Menu-center-relative px. Where the Tile is
    /// *drawn* can differ while a spring is still catching up or a drag is in
    /// flight; this is where it belongs, which is what hit-testing wants.
    pub fn tile_center(&self, k: usize) -> [f32; 2] {
        let a = self.slot_angle(k);
        [a.cos() * self.rest_r(), a.sin() * self.rest_r()]
    }

    /// Center of Slot `k`'s remove control — the X in the Tile's top-right
    /// corner, live only while Pinned.
    pub fn remove_center(&self, k: usize) -> [f32; 2] {
        let c = self.tile_center(k);
        [c[0] + self.tile_half * 0.85, c[1] - self.tile_half * 0.85]
    }

    /// Radius of the remove control. Small on purpose: it is the only
    /// irreversible action in the app and it has no confirmation step.
    pub fn remove_r(&self) -> f32 {
        (self.tile_half * 0.28).clamp(7.0, 12.0)
    }

    pub fn on_remove(&self, p: (f32, f32), k: usize) -> bool {
        let c = self.remove_center(k);
        let (dx, dy) = (p.0 - c[0], p.1 - c[1]);
        let r = self.remove_r();
        dx * dx + dy * dy <= r * r
    }

    /// The Dodaj slot's toggle, centered in the Hub. Sized to the switch plus
    /// its caption, which is what `hub_r_pinned` is floored to fit.
    pub fn toggle_half(&self) -> [f32; 2] {
        [36.0, 20.0]
    }

    pub fn on_toggle(&self, p: (f32, f32)) -> bool {
        let h = self.toggle_half();
        p.0.abs() <= h[0] && p.1.abs() <= h[1]
    }

    /// The Done button's chord — the same bottom segment the Gear zone uses,
    /// measured against the Pinned Hub's radius. The way in and the way out sit
    /// in the same place, so the button is where the user last clicked.
    pub fn done_cut_dy(&self) -> f32 {
        self.hub_r_pinned() * 0.6
    }

    /// Inside the Done button: leaving Pinned. Cannot overlap the toggle — the
    /// chord sits below the toggle's bottom edge at every Hub size, which the
    /// tests pin down.
    pub fn on_done(&self, p: (f32, f32)) -> bool {
        let r = self.hub_r_pinned();
        p.0 * p.0 + p.1 * p.1 < r * r && p.1 > self.done_cut_dy()
    }

    /// Square window edge length — headroom has to grow with tile/label size
    /// or a large configured tile/label clips at the window edge.
    /// ADR-0002: the window never resizes at runtime, so it must fit the
    /// Popover panel from the start, even under a tiny configured radius.
    pub fn window_size(&self) -> u32 {
        let margin = self.tile_half + self.label_font_px * 3.0 + 20.0;
        let panel_floor = (popover::PANEL_W.max(popover::PANEL_H) + 24.0) as u32;
        (2 * (self.scrim_r + margin) as u32).max(panel_floor)
    }
}

/// One of the three Now Playing controls in the Hub's upper region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportButton {
    Prev,
    PlayPause,
    Next,
}

/// Where Item `i` ends up when the Item at `from` is moved to `to`.
///
/// This is `remove(from)` + `insert(to)` expressed as a pure index map, so the
/// live drag preview and the eventual write agree by construction — the Tiles
/// slide to exactly the arrangement the drop will produce.
pub fn moved_index(i: usize, from: usize, to: usize) -> usize {
    if i == from {
        to
    } else if from < to && i > from && i <= to {
        i - 1
    } else if from > to && i >= to && i < from {
        i + 1
    } else {
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `slots` total, Dodaj slot last — matches the old free-function tests.
    fn geo(slots: usize) -> MenuGeometry {
        geo_with(slots - 1, Some(AddPosition::Last))
    }

    /// hub_r = 200 * 0.2 = 40.
    fn geo_with(item_count: usize, add_at: Option<AddPosition>) -> MenuGeometry {
        MenuGeometry {
            scrim_r: 200.0,
            tile_half: 32.0,
            hub_ratio: 0.2,
            label_font_px: 13.0,
            item_count,
            add_at,
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

    #[test]
    fn window_always_fits_the_popover_panel() {
        // Minimal clamped config (radius 80, tiles 16, labels 8) would yield a
        // 280px window — smaller than the 320px panel. The floor must win.
        let g = MenuGeometry {
            scrim_r: 80.0,
            tile_half: 16.0,
            hub_ratio: 0.2,
            label_font_px: 8.0,
            item_count: 1,
            add_at: Some(AddPosition::Last),
        };
        let panel = popover::PANEL_W.max(popover::PANEL_H) as u32;
        assert!(g.window_size() > panel);
        // A roomy config is untouched by the floor.
        assert_eq!(geo(5).window_size(), 2 * (200 + 32 + 39 + 20));
    }

    /// Slot index and Item index are the same number only while the Dodaj slot
    /// is last (or gone). Put it first and everything shifts by one.
    #[test]
    fn slots_map_to_items_around_the_dodaj_slot() {
        let last = geo_with(3, Some(AddPosition::Last));
        assert_eq!(last.slot_count(), 4);
        assert_eq!(last.meta_slot(), Some(3));
        assert_eq!(last.item_at(0), Some(0));
        assert_eq!(last.item_at(2), Some(2));
        assert_eq!(last.item_at(3), None); // the meta slot shows no Item
        assert_eq!(last.slot_of_item(2), 2);

        let first = geo_with(3, Some(AddPosition::First));
        assert_eq!(first.slot_count(), 4);
        assert_eq!(first.meta_slot(), Some(0));
        assert_eq!(first.item_at(0), None);
        assert_eq!(first.item_at(1), Some(0));
        assert_eq!(first.item_at(3), Some(2));
        assert_eq!(first.slot_of_item(0), 1);

        let hidden = geo_with(3, None);
        assert_eq!(hidden.slot_count(), 3);
        assert_eq!(hidden.meta_slot(), None);
        assert_eq!(hidden.item_at(0), Some(0));
        assert_eq!(hidden.item_at(2), Some(2));
        assert_eq!(hidden.item_at(3), None); // past the end

        // Round trip both ways, whatever the Dodaj slot is doing.
        for g in [last, first, hidden] {
            for i in 0..g.item_count() {
                assert_eq!(g.item_at(g.slot_of_item(i)), Some(i));
            }
        }
    }

    /// Dropping on the meta sector must land somewhere, not nowhere — and never
    /// past the end of the Item list.
    #[test]
    fn drop_index_clamps_the_meta_sector_to_an_item() {
        let last = geo_with(3, Some(AddPosition::Last));
        assert_eq!(last.drop_index(1), 1);
        assert_eq!(last.drop_index(3), 2); // meta sector -> last Item

        let first = geo_with(3, Some(AddPosition::First));
        assert_eq!(first.drop_index(0), 0); // meta sector -> first Item
        assert_eq!(first.drop_index(2), 1);

        // Every sector, every layout: always a valid index into the Items.
        for g in [last, first, geo_with(3, None)] {
            for slot in 0..g.slot_count() {
                assert!(g.drop_index(slot) < g.item_count());
            }
        }
        // One lone Item: every sector is that Item, no underflow.
        let one = geo_with(1, Some(AddPosition::Last));
        assert_eq!(one.drop_index(0), 0);
        assert_eq!(one.drop_index(1), 0);
    }

    /// No Items and a hidden Dodaj slot is a legal Menu, not a crash: the old
    /// `slot_count - 1` underflowed and the angle formula divided by zero.
    #[test]
    fn an_empty_menu_has_no_slots_and_no_nan() {
        let g = geo_with(0, None);
        assert_eq!(g.slot_count(), 0);
        assert_eq!(g.meta_slot(), None);
        assert_eq!(g.item_at(0), None);
        assert!(g.slot_angle(0).is_finite());
        assert_eq!(g.hovered_slot((0.0, -200.0), (0.0, 0.0)), None);
        // The Gear zone is carved out of the Hub, so it survives an empty Menu
        // — which is the only way back to a Dodaj slot the user hid.
        assert!(g.in_gear_zone((0.0, 30.0), (0.0, 0.0)));
        // Nothing to drop onto, but still no underflow.
        assert_eq!(g.drop_index(0), 0);
    }

    /// The Pinned floor must not leak into `hub_r`: that number is also the
    /// Dead zone and the Gear zone, and moving it would change where releasing
    /// the Trigger cancels.
    #[test]
    fn the_pinned_hub_floor_leaves_the_dead_zone_alone() {
        // hub_r = 200 * 0.2 = 40, just under the floor.
        let g = geo(5);
        assert_eq!(g.hub_r(), 40.0);
        assert!(g.hub_r_pinned() > g.hub_r());

        // A point between the two radii: outside the Dead zone, so it still
        // selects a Slot exactly as before.
        let between = (0.0, -(g.hub_r() as f64) - 2.0);
        assert!(between.1.abs() < g.hub_r_pinned() as f64);
        assert_eq!(g.hovered_slot(between, (0.0, 0.0)), Some(0));
        assert!(!g.in_gear_zone((0.0, 42.0), (0.0, 0.0)));

        // A roomy Hub is already past the floor and untouched by it.
        let roomy = geo_with(4, Some(AddPosition::Last));
        let roomy = MenuGeometry {
            hub_ratio: 0.4,
            ..roomy
        };
        assert_eq!(roomy.hub_r_pinned(), roomy.hub_r());
    }

    /// The Done button sits where the Gear zone was, so the way out is where
    /// the way in was — and it must never eat the toggle's clicks.
    #[test]
    fn done_button_replaces_the_gear_zone_without_hitting_the_toggle() {
        for (scrim, ratio) in [(200.0, 0.2), (80.0, 0.05), (600.0, 0.5), (280.0, 0.28)] {
            let g = MenuGeometry {
                scrim_r: scrim,
                hub_ratio: ratio,
                ..geo(5)
            };
            let r = g.hub_r_pinned();
            // Bottom of the Pinned Hub: the button.
            assert!(g.on_done((0.0, r - 1.0)), "scrim {scrim} ratio {ratio}");
            // Center and above the chord: not the button.
            assert!(!g.on_done((0.0, 0.0)));
            assert!(!g.on_done((0.0, g.done_cut_dy() - 1.0)));
            // Outside the Hub entirely: not the button.
            assert!(!g.on_done((r + 5.0, r)));

            // The toggle owns the middle; the two must not overlap anywhere.
            let h = g.toggle_half();
            for x in [-h[0], 0.0, h[0]] {
                for y in [-h[1], 0.0, h[1]] {
                    assert!(
                        !(g.on_toggle((x, y)) && g.on_done((x, y))),
                        "toggle and done overlap at ({x}, {y}), scrim {scrim} ratio {ratio}"
                    );
                }
            }
        }
    }

    /// The index map has to agree with what `remove` + `insert` actually does,
    /// or the drag preview shows one arrangement and the drop writes another.
    #[test]
    fn moved_index_matches_remove_then_insert() {
        for len in 1..7usize {
            for from in 0..len {
                for to in 0..len {
                    let mut v: Vec<usize> = (0..len).collect();
                    let x = v.remove(from);
                    v.insert(to, x);
                    for (i, _) in (0..len).enumerate() {
                        let predicted = moved_index(i, from, to);
                        assert_eq!(
                            v[predicted], i,
                            "len {len}, {from}->{to}: item {i} predicted at {predicted}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn transport_buttons_split_the_hub_left_to_right() {
        let g = geo(5); // hub_r = 40, gear chord at +24
        assert_eq!(g.transport_button((-30.0, 0.0)), Some(TransportButton::Prev));
        assert_eq!(g.transport_button((0.0, 0.0)), Some(TransportButton::PlayPause));
        assert_eq!(g.transport_button((30.0, 0.0)), Some(TransportButton::Next));
        // Outside the Hub entirely: no button.
        assert_eq!(g.transport_button((0.0, -100.0)), None);
        // Inside the Gear zone (below the chord): not a Transport button.
        assert_eq!(g.transport_button((0.0, 30.0)), None);
    }

    #[test]
    fn transport_buttons_never_reach_into_the_gear_zone() {
        // Every point the Gear zone claims must be a non-button, at several
        // Hub sizes, so the two controls never fight over the same pixel.
        for (scrim, ratio) in [(200.0, 0.2), (80.0, 0.05), (600.0, 0.5)] {
            let g = MenuGeometry { scrim_r: scrim, hub_ratio: ratio, ..geo(5) };
            let r = g.hub_r();
            for i in 0..20 {
                let a = i as f32 / 20.0 * TAU;
                let p = (a.cos() * r * 0.99, a.sin() * r * 0.99);
                if g.in_gear_zone((p.0 as f64, p.1 as f64), (0.0, 0.0)) {
                    assert!(g.transport_button(p).is_none(), "{p:?} scrim {scrim} ratio {ratio}");
                }
            }
        }
    }

    #[test]
    fn title_arc_sits_above_the_transport_buttons() {
        let g = geo(5); // hub_r = 40
        // Straight up, inside the ring: the arc's own region.
        assert!(g.on_title_arc((0.0, -30.0)));
        // Same point is never also read as a Transport button.
        assert_eq!(g.transport_button((0.0, -30.0)), None);
        // Dead center and the Gear zone: not the title arc.
        assert!(!g.on_title_arc((0.0, 0.0)));
        assert!(!g.on_title_arc((0.0, 30.0)));
        // Outside the Hub entirely: not the title arc.
        assert!(!g.on_title_arc((0.0, -100.0)));
    }

    fn angle_point(deg: f32, r: f32) -> (f64, f64) {
        let rad = deg.to_radians();
        (rad.cos() as f64 * r as f64, rad.sin() as f64 * r as f64)
    }
}
