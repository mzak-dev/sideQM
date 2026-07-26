//! Spring/Phase animation engine, free of GPU/text/windowing: `Animator`
//! ticks with a caller-supplied `dt` and returns a `FrameModel` that
//! `gfx::draw` renders. No wall clock in here — `Phase::Shown` accumulates
//! elapsed time from `dt`, so every behaviour is deterministic under test.

use std::f32::consts::{PI, TAU};

use crate::config::Animation;
use crate::geometry::MenuGeometry;

// --- tuning ---
// (durations are spring response times in seconds; omega = 4 / (zeta * response))
/// Hover select spring: bouncy, shares the open damping.
const SELECT_RESPONSE_S: f32 = 0.5;
/// Deselect spring: critically damped.
const DESELECT_RESPONSE_S: f32 = 0.4;
/// Launch pop: spec scales the hovered tile 1.16 -> 1.42 at launch.
const POP_RATIO: f32 = 1.42 / 1.16;
/// The Scrim's entrance starts once this fraction of the Stagger sequence has begun.
const SCRIM_GATE: f32 = 0.6;
/// Arc shortest-path rotation: bouncy, shares the open damping.
const ARC_RESPONSE_S: f32 = 0.45;
/// Arc fade, in and out: critically damped.
const ARC_FADE_S: f32 = 0.15;
/// Popover expand/collapse spring, shares the open damping.
const POPOVER_RESPONSE_S: f32 = 0.35;

/// Damped spring toward a target; the whole animation system is these.
#[derive(Clone, Copy, Default)]
struct Spring {
    x: f32,
    v: f32,
}

/// Integration steps are kept under this much spring travel (omega * step).
/// Symplectic Euler diverges once omega * step approaches 2, and a diverged
/// spring is not a cosmetic problem: `scale` reaches the renderer as infinite
/// quad extents, every Tile covers the screen, and the resulting overdraw can
/// take the display driver down with it.
const MAX_SPRING_STEP: f32 = 0.2;

impl Spring {
    fn tick(&mut self, target: f32, omega: f32, zeta: f32, dt: f32) {
        // A stiff spring (short response) or a long frame needs more than one
        // step to stay inside the stable region. Config clamping keeps omega
        // bounded, so the step ceiling is never actually reached.
        let steps = ((omega * dt / MAX_SPRING_STEP).ceil() as i32).clamp(1, 64);
        let h = dt / steps as f32;
        for _ in 0..steps {
            let accel = -2.0 * zeta * omega * self.v - omega * omega * (self.x - target);
            self.v += accel * h;
            self.x += self.v * h;
        }
    }

    fn settled(&self, target: f32) -> bool {
        (self.x - target).abs() < 0.005 && self.v.abs() < 0.05
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Closed,
    /// Opening and open are one phase; the springs decide when motion stops.
    /// `elapsed` accumulates ticked `dt`, driving the Stagger and Scrim gate.
    Shown {
        elapsed: f32,
    },
    Closing,
}

pub struct SlotFrame {
    /// Tile scale: entrance spring x select spring.
    pub scale: f32,
    /// Tile opacity, from the entrance spring alone.
    pub alpha: f32,
}

/// Everything the renderer needs for one frame, as plain data.
pub struct FrameModel {
    pub slots: Vec<SlotFrame>,
    /// Scrim entrance progress, 0..~1 (can overshoot when bouncy).
    pub scrim: f32,
    /// Arc pointing angle + opacity, when visible at all.
    pub arc: Option<(f32, f32)>,
    /// The Hover this frame was ticked with (drives select colors + Hub text).
    pub hovered: Option<usize>,
    /// Popover expand progress, 0..~1 (can overshoot when bouncy).
    pub popover: f32,
    /// Keep the redraw loop running. False also means: nothing to draw.
    pub request_frame: bool,
    /// The close animation just finished; hide the window now.
    pub just_closed: bool,
}

impl FrameModel {
    fn idle(hovered: Option<usize>, just_closed: bool) -> FrameModel {
        FrameModel {
            slots: Vec::new(),
            scrim: 0.0,
            arc: None,
            hovered,
            popover: 0.0,
            request_frame: false,
            just_closed,
        }
    }
}

pub struct Animator {
    phase: Phase,
    tile_springs: Vec<Spring>,
    scrim_spring: Spring,
    /// Per-slot selected-tile scale (1.0 <-> hover_scale), independent of the
    /// tile's own entrance/exit spring.
    select_springs: Vec<Spring>,
    arc_rot: Spring,
    arc_alpha: Spring,
    /// Unwrapped rotation target the arc springs toward (can exceed +-TAU so
    /// retargeting always takes the shortest path, never the long way around).
    arc_target: f32,
    /// Whether the arc is currently shown (or fading); false means the next
    /// appearance should snap instead of springing from a stale angle.
    arc_on: bool,
    last_hover: Option<usize>,
    /// Captured at begin_close so the launched tile can pop while everything
    /// else fades, even though `hover` itself is cleared right after.
    closing_launched: Option<usize>,
    /// Popover expansion toward 1 while Pinned, back toward 0 otherwise.
    popover_spring: Spring,
    pinned: bool,
}

impl Animator {
    pub fn new() -> Animator {
        Animator {
            phase: Phase::Closed,
            tile_springs: Vec::new(),
            scrim_spring: Spring::default(),
            select_springs: Vec::new(),
            arc_rot: Spring::default(),
            arc_alpha: Spring::default(),
            arc_target: 0.0,
            arc_on: false,
            last_hover: None,
            closing_launched: None,
            popover_spring: Spring::default(),
            pinned: false,
        }
    }

    /// The Popover starts expanding out of the Dodaj Tile (Pinned state).
    pub fn begin_pin(&mut self) {
        self.pinned = true;
    }

    /// Pinned ended (commit, discard, or Trigger restart): collapse the Popover.
    pub fn end_pin(&mut self) {
        self.pinned = false;
    }

    /// Resize per-slot springs after the Slot count changed (config reload).
    pub fn set_slot_count(&mut self, n: usize) {
        self.tile_springs = vec![Spring::default(); n];
        self.select_springs = vec![Spring::default(); n];
    }

    /// Start (or restart) the entrance. Reopening mid-close keeps the current
    /// spring state so the menu springs back instead of popping.
    pub fn begin_open(&mut self) {
        if matches!(self.phase, Phase::Closed) {
            for s in &mut self.tile_springs {
                *s = Spring::default();
            }
            for s in &mut self.select_springs {
                *s = Spring::default();
            }
            self.scrim_spring = Spring::default();
            self.arc_rot = Spring::default();
            self.arc_alpha = Spring::default();
            self.arc_on = false;
            self.last_hover = None;
            self.closing_launched = None;
            self.popover_spring = Spring::default();
            self.pinned = false;
        }
        self.phase = Phase::Shown { elapsed: 0.0 };
    }

    /// Start the collective shrink+fade. Launching already happened; this is
    /// cosmetic. `launched` is the slot that fired, if any, captured here
    /// because `hover` itself gets cleared by the caller right after this call.
    pub fn begin_close(&mut self, launched: Option<usize>) {
        if !matches!(self.phase, Phase::Closed) {
            self.phase = Phase::Closing;
            self.closing_launched = launched;
        }
    }

    /// Advance all springs by `dt` seconds and describe the resulting frame.
    pub fn tick(
        &mut self,
        dt: f32,
        hover: Option<usize>,
        geo: &MenuGeometry,
        cfg: &Animation,
    ) -> FrameModel {
        let n = self.tile_springs.len().max(1);
        // Accessors, not fields: these come from user-edited JSON and a stiff
        // enough spring diverges (see MAX_SPRING_STEP).
        let (open_ms, close_ms) = (cfg.open_ms(), cfg.close_ms());
        let zeta_open = (1.0 - 0.6 * cfg.bounciness()).max(0.15);
        let omega_open = 4.0 / (zeta_open * (open_ms.max(1) as f32 / 1000.0));
        let omega_close = 4.0 / (close_ms.max(1) as f32 / 1000.0);
        let stagger = cfg.stagger_ms() as f32 / 1000.0;

        if let Phase::Shown { elapsed } = &mut self.phase {
            *elapsed += dt;
        }
        match self.phase {
            Phase::Closed => return FrameModel::idle(hover, false),
            Phase::Shown { elapsed } => {
                for (k, s) in self.tile_springs.iter_mut().enumerate() {
                    if open_ms == 0 {
                        *s = Spring { x: 1.0, v: 0.0 };
                    } else if elapsed >= k as f32 * stagger {
                        s.tick(1.0, omega_open, zeta_open, dt);
                    }
                }
                // The Scrim starts once ~60% of the tiles have begun landing.
                if open_ms == 0 {
                    self.scrim_spring = Spring { x: 1.0, v: 0.0 };
                } else if elapsed >= SCRIM_GATE * n as f32 * stagger {
                    self.scrim_spring.tick(1.0, omega_open, zeta_open, dt);
                }
            }
            Phase::Closing => {
                let mut all_settled = true;
                for s in &mut self.tile_springs {
                    if close_ms == 0 {
                        *s = Spring::default();
                    } else {
                        s.tick(0.0, omega_close, 1.0, dt);
                    }
                    all_settled &= s.settled(0.0);
                }
                if close_ms == 0 {
                    self.scrim_spring = Spring::default();
                } else {
                    self.scrim_spring.tick(0.0, omega_close, 1.0, dt);
                }
                all_settled &= self.scrim_spring.settled(0.0);
                if all_settled {
                    self.phase = Phase::Closed;
                    return FrameModel::idle(hover, true);
                }
            }
        }

        // --- selection: per-tile select-scale, springing at a different rate
        // toward hover_scale than away from it, plus a bigger "launched" pop ---
        let omega_select = 4.0 / (zeta_open * SELECT_RESPONSE_S);
        let omega_deselect = 4.0 / DESELECT_RESPONSE_S;
        let pop_scale = cfg.hover_scale() * POP_RATIO;
        for (k, s) in self.select_springs.iter_mut().enumerate() {
            let target = if matches!(self.phase, Phase::Closing) && self.closing_launched == Some(k)
            {
                pop_scale
            } else if hover == Some(k) {
                cfg.hover_scale()
            } else {
                1.0
            };
            if target > s.x {
                s.tick(target, omega_select, zeta_open, dt);
            } else {
                s.tick(target, omega_deselect, 1.0, dt);
            }
        }

        // --- arc: snap on first appearance, spring by the shortest angular
        // path after that ---
        if hover != self.last_hover {
            match hover {
                Some(k) => {
                    let target_angle = geo.slot_angle(k);
                    if self.arc_on {
                        let delta = ((target_angle - self.arc_target + PI).rem_euclid(TAU)) - PI;
                        self.arc_target += delta;
                    } else {
                        self.arc_target = target_angle;
                        self.arc_rot = Spring {
                            x: target_angle,
                            v: 0.0,
                        };
                        self.arc_on = true;
                    }
                }
                None => self.arc_on = false,
            }
            self.last_hover = hover;
        }
        let omega_arc = 4.0 / (zeta_open * ARC_RESPONSE_S);
        let omega_arc_fade = 4.0 / ARC_FADE_S;
        self.arc_rot.tick(self.arc_target, omega_arc, zeta_open, dt);
        self.arc_alpha.tick(
            if hover.is_some() { 1.0 } else { 0.0 },
            omega_arc_fade,
            1.0,
            dt,
        );

        let omega_popover = 4.0 / (zeta_open * POPOVER_RESPONSE_S);
        self.popover_spring.tick(
            if self.pinned { 1.0 } else { 0.0 },
            omega_popover,
            zeta_open,
            dt,
        );

        let slots = self
            .tile_springs
            .iter()
            .zip(&self.select_springs)
            .map(|(tile, select)| {
                let intro = tile.x.max(0.0);
                SlotFrame {
                    scale: intro * select.x,
                    alpha: intro.clamp(0.0, 1.0),
                }
            })
            .collect();
        let arc_alpha = self.arc_alpha.x.clamp(0.0, 1.0);
        FrameModel {
            slots,
            scrim: self.scrim_spring.x,
            arc: (arc_alpha > 0.01).then_some((self.arc_rot.x, arc_alpha)),
            hovered: hover,
            popover: self.popover_spring.x.max(0.0),
            request_frame: true,
            just_closed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Appearance;
    use std::f32::consts::FRAC_PI_2;

    fn geo(item_count: usize) -> MenuGeometry {
        MenuGeometry::new(&Appearance::default(), item_count)
    }

    fn cfg() -> Animation {
        Animation {
            open_ms: 480,
            close_ms: 180,
            stagger_ms: 100,
            bounciness: 0.0,
            hover_scale: 1.16,
        }
    }

    fn opened(n_slots: usize) -> Animator {
        let mut a = Animator::new();
        a.set_slot_count(n_slots);
        a.begin_open();
        a
    }

    /// Tick in fixed 10ms steps for `secs`, returning the last frame.
    fn run(
        a: &mut Animator,
        g: &MenuGeometry,
        c: &Animation,
        hover: Option<usize>,
        secs: f32,
    ) -> FrameModel {
        let steps = (secs / 0.01).round() as usize;
        let mut last = a.tick(0.0, hover, g, c);
        for _ in 0..steps {
            last = a.tick(0.01, hover, g, c);
        }
        last
    }

    #[test]
    fn stagger_delays_later_slots() {
        // 4 slots, 100ms stagger: at t=150ms slot 0 has moved, slot 2 (gate
        // 200ms) has not.
        let (g, c) = (geo(3), cfg());
        let mut a = opened(4);
        let frame = run(&mut a, &g, &c, None, 0.15);
        assert!(frame.slots[0].alpha > 0.0);
        assert_eq!(frame.slots[2].alpha, 0.0);
    }

    #[test]
    fn scrim_waits_for_the_stagger_gate() {
        // Gate = 0.6 * 4 slots * 100ms = 240ms: still zero at 150ms, moving by 350ms.
        let (g, c) = (geo(3), cfg());
        let mut a = opened(4);
        let early = run(&mut a, &g, &c, None, 0.15);
        assert_eq!(early.scrim, 0.0);
        let later = run(&mut a, &g, &c, None, 0.2);
        assert!(later.scrim > 0.0);
    }

    #[test]
    fn instant_open_skips_the_entrance() {
        let g = geo(3);
        let c = Animation {
            open_ms: 0,
            ..cfg()
        };
        let mut a = opened(4);
        let frame = a.tick(0.01, None, &g, &c);
        assert!(frame.slots.iter().all(|s| s.alpha == 1.0));
        assert_eq!(frame.scrim, 1.0);
    }

    #[test]
    fn arc_retargets_by_the_shortest_path() {
        // 4 slots at -90/0/90/180 degrees. Hover slot 0 (snap to -PI/2), then
        // slot 3 (+180): the long way is +270 degrees, the short way -90, so
        // the unwrapped target must land at -PI, not +PI.
        let (g, c) = (geo(3), cfg());
        let mut a = opened(4);
        a.tick(0.01, Some(0), &g, &c);
        assert!((a.arc_target - (-FRAC_PI_2)).abs() < 1e-5);
        a.tick(0.01, Some(3), &g, &c);
        assert!((a.arc_target - (-PI)).abs() < 1e-5);
    }

    #[test]
    fn close_reports_just_closed_exactly_once() {
        let g = geo(3);
        let c = Animation {
            close_ms: 0,
            ..cfg()
        };
        let mut a = opened(4);
        a.tick(0.01, None, &g, &cfg());
        a.begin_close(None);
        let closing = a.tick(0.01, None, &g, &c);
        assert!(closing.just_closed);
        assert!(!closing.request_frame);
        let after = a.tick(0.01, None, &g, &c);
        assert!(!after.just_closed);
        assert!(!after.request_frame);
    }

    #[test]
    fn popover_spring_expands_while_pinned_and_collapses_after() {
        let (g, c) = (geo(3), cfg());
        let mut a = opened(4);
        assert_eq!(run(&mut a, &g, &c, None, 0.5).popover, 0.0);
        a.begin_pin();
        let frame = run(&mut a, &g, &c, None, 1.5);
        assert!((frame.popover - 1.0).abs() < 0.05);
        a.end_pin();
        let frame = run(&mut a, &g, &c, None, 1.5);
        assert!(frame.popover < 0.05);
    }

    /// Hostile config values must never reach the renderer as inf/NaN: a tile
    /// scale of infinity draws every Tile over the whole screen, and that much
    /// overdraw in the SDF shader can reset the display driver.
    #[test]
    fn absurd_animation_config_still_produces_finite_frames() {
        let g = geo(11);
        let hostile = [
            // A 1 ms response is stiff enough to diverge single-step Euler.
            Animation { open_ms: 1, close_ms: 1, stagger_ms: 0, bounciness: 0.0, hover_scale: 1.16 },
            Animation { open_ms: 1, close_ms: 1, stagger_ms: 0, bounciness: 1.0, hover_scale: 1.16 },
            Animation { open_ms: 2, close_ms: 3, stagger_ms: u32::MAX, bounciness: 5.0, hover_scale: 1e30 },
            Animation {
                open_ms: 1,
                close_ms: 1,
                stagger_ms: 1,
                bounciness: f32::NAN,
                hover_scale: f32::NAN,
            },
        ];
        for c in hostile {
            let mut a = opened(12);
            // Long frames too: dt is capped at 0.05 by the render loop.
            for _ in 0..400 {
                let f = a.tick(0.05, Some(3), &g, &c);
                assert!(f.scrim.is_finite(), "scrim diverged");
                assert!(f.popover.is_finite(), "popover diverged");
                if let Some((angle, alpha)) = f.arc {
                    assert!(angle.is_finite() && alpha.is_finite(), "arc diverged");
                }
                for (k, s) in f.slots.iter().enumerate() {
                    assert!(s.scale.is_finite(), "slot {k} scale diverged");
                    assert!(s.alpha.is_finite(), "slot {k} alpha diverged");
                    // Also bounded: a huge-but-finite scale is just as fatal.
                    assert!(s.scale.abs() < 100.0, "slot {k} scale ran away: {}", s.scale);
                }
            }
        }
    }

    #[test]
    fn spring_settles_on_target() {
        let mut s = Spring::default();
        for _ in 0..1000 {
            s.tick(1.0, 10.0, 1.0, 0.01);
        }
        assert!(s.settled(1.0));
        assert!((s.x - 1.0).abs() < 0.01);
    }
}
