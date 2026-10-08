//! Rail placement: keep the window fully inside a monitor work area.
//!
//! One visibility rule serves three entry points: the startup restore
//! (`place`), the post-drag watchdog (`plan_at` + `glide`) and the settings
//! reset (`reset_plan` + `glide`).
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{LogicalPosition, Monitor, WebviewWindow};

/// Slack for the summed-area comparison. It absorbs floating-point error in the
/// intersection sum, not a physical-pixel round trip (see `MOVE_DEADBAND`).
const COVERAGE_EPSILON: f64 = 1e-3;
/// `set_position` emits `Moved`. A fractional-DPI round trip is under one
/// logical pixel; writing that again would schedule another pass forever.
const MOVE_DEADBAND: f64 = 1.0;
/// Default placement: 12 px from the work area's right edge, 80 px from its top.
const RIGHT_MARGIN: f64 = 12.0;
const TOP_MARGIN: f64 = 80.0;
/// Corrective moves are animated: about 200 ms of ease-out, one frame per 16 ms.
const GLIDE_MS: u64 = 200;
const FRAME_MS: u64 = 16;
/// Reset bumps this so an in-flight correction exits; a newer reset preempts
/// an older one. Corrections snapshot it *before* queueing `plan_at`, otherwise
/// a later `glide` would start from a stale off-screen `from`.
static ANIMATION_SEQ: AtomicU64 = AtomicU64::new(0);

fn snapshot(seq: &AtomicU64) -> u64 {
    seq.load(Ordering::SeqCst)
}

fn bump(seq: &AtomicU64) -> u64 {
    seq.fetch_add(1, Ordering::SeqCst) + 1
}

fn is_current(seq: &AtomicU64, token: u64) -> bool {
    seq.load(Ordering::SeqCst) == token
}

/// Caller's snapshot of the animation generation, taken before queueing `plan`.
pub fn animation_generation() -> u64 {
    snapshot(&ANIMATION_SEQ)
}

/// A rectangle in logical points, the coordinate space the saved position file
/// and `set_position` both use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn area(&self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }
}

/// `None` when the window is fully covered by the union of the work areas;
/// otherwise the corrected top-left corner.
///
/// Overlapping areas (mirrored displays) are counted twice, which only makes
/// the check more lenient: a window that is visible either way stays put.
pub fn clamp_into_view(window: Rect, areas: &[Rect]) -> Option<(f64, f64)> {
    // Non-finite or empty input has no safe target. `None` means "do not move".
    if !finite_rect(window) {
        return None;
    }
    let areas: Vec<Rect> = areas.iter().copied().filter(usable_area).collect();
    if areas.is_empty() {
        return None;
    }
    let covered: f64 = areas
        .iter()
        .map(|area| intersection(window, *area).area())
        .sum();
    if covered >= window.area() - COVERAGE_EPSILON {
        return None;
    }
    let target = areas
        .iter()
        .copied()
        .max_by(|a, b| {
            intersection(window, *a)
                .area()
                .total_cmp(&intersection(window, *b).area())
        })
        .unwrap();
    // No overlap at all: fall back to the work area with the closest center.
    let target = if intersection(window, target).area() > 0.0 {
        target
    } else {
        areas
            .iter()
            .copied()
            .min_by(|a, b| distance2(window, *a).total_cmp(&distance2(window, *b)))
            .unwrap()
    };
    Some((
        axis(window.x, target.x, target.width, window.width),
        axis(window.y, target.y, target.height, window.height),
    ))
}

/// Top-right default: `RIGHT_MARGIN` from the right edge, `TOP_MARGIN` from the
/// top. The caller clamps the result, so this stays a plain candidate; the
/// height only matters to that clamp, never to the corner itself.
pub fn default_position(area: Rect, width: f64, _height: f64) -> (f64, f64) {
    (area.x + area.width - width - RIGHT_MARGIN, area.y + TOP_MARGIN)
}

/// Startup restore: `None` uses the default position, and the candidate always
/// passes through the clamp before it is applied.
pub fn place(window: &WebviewWindow, saved: Option<(f64, f64)>) -> Result<(), String> {
    let size = window_size(window)?;
    let areas = work_areas(window)?;
    let candidate = match saved {
        Some((x, y)) if x.is_finite() && y.is_finite() => (x, y),
        _ => default_position(primary_work_area(window)?, size.0, size.1),
    };
    move_to(window, candidate, size, &areas)
}

/// A planned corrective move: where the window is and where it must go.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Move {
    pub from: (f64, f64),
    pub to: (f64, f64),
    seq: u64,
}

/// Main-thread read for the watchdog: `None` when the window is already fully
/// visible, when the gap is under the move deadband, or when `generation` is
/// no longer current (a reset started after the caller sampled the counter).
pub fn plan_at(window: &WebviewWindow, generation: u64) -> Result<Option<Move>, String> {
    if !is_current(&ANIMATION_SEQ, generation) {
        return Ok(None);
    }
    let rect = current_rect(window)?;
    match clamp_into_view(rect, &work_areas(window)?) {
        None => Ok(None),
        Some((x, y)) if !correction_is_material(rect.x, rect.y, x, y) => Ok(None),
        Some((x, y)) => Ok(Some(Move { from: (rect.x, rect.y), to: (x, y), seq: generation })),
    }
}

/// Main-thread read for the reset command: the primary work area's top-right
/// corner, clamped as a fallback. Always returns a target. Bumps the
/// generation so any in-flight or already-planned correction exits.
pub fn reset_plan(window: &WebviewWindow) -> Result<Move, String> {
    let rect = current_rect(window)?;
    let size = (rect.width, rect.height);
    let candidate = default_position(primary_work_area(window)?, size.0, size.1);
    let target = Rect { x: candidate.0, y: candidate.1, width: size.0, height: size.1 };
    let (x, y) = clamp_into_view(target, &work_areas(window)?).unwrap_or(candidate);
    Ok(Move { from: (rect.x, rect.y), to: (x, y), seq: bump(&ANIMATION_SEQ) })
}

/// Animates `mv` at roughly 60 fps. `set_position` dispatches to the main
/// thread asynchronously on every backend, so this may run on any thread.
pub fn glide(window: &WebviewWindow, mv: Move) {
    if mv.from == mv.to {
        return;
    }
    let steps = (GLIDE_MS / FRAME_MS).max(1);
    for step in 1..=steps {
        if !is_current(&ANIMATION_SEQ, mv.seq) {
            return;
        }
        let t = step as f64 / steps as f64;
        let (x, y) = glide_point(mv.from, mv.to, t);
        if window.set_position(LogicalPosition::new(x, y)).is_err() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(FRAME_MS));
    }
}

/// Ease-out cubic: fast at first, settling on the target.
fn glide_point(from: (f64, f64), to: (f64, f64), t: f64) -> (f64, f64) {
    let eased = 1.0 - (1.0 - t).powi(3);
    (from.0 + (to.0 - from.0) * eased, from.1 + (to.1 - from.1) * eased)
}

fn current_rect(window: &WebviewWindow) -> Result<Rect, String> {
    let scale = window_scale(window)?;
    let position = window
        .outer_position()
        .map_err(|e| e.to_string())?
        .to_logical::<f64>(scale);
    let size = window_size(window)?;
    Ok(Rect { x: position.x, y: position.y, width: size.0, height: size.1 })
}

fn move_to(window: &WebviewWindow, candidate: (f64, f64), size: (f64, f64), areas: &[Rect]) -> Result<(), String> {
    let rect = Rect { x: candidate.0, y: candidate.1, width: size.0, height: size.1 };
    let (x, y) = clamp_into_view(rect, areas).unwrap_or(candidate);
    window.set_position(LogicalPosition::new(x, y)).map_err(|e| e.to_string())
}

fn window_scale(window: &WebviewWindow) -> Result<f64, String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    // `PhysicalPosition::to_logical` asserts on a non-normal scale and would
    // take down the main thread from the correction worker.
    if scale.is_sign_positive() && scale.is_normal() {
        Ok(scale)
    } else {
        Err("无法读取窗口缩放".into())
    }
}

fn window_size(window: &WebviewWindow) -> Result<(f64, f64), String> {
    let scale = window_scale(window)?;
    let size = window.outer_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    if size.width.is_finite() && size.height.is_finite() && size.width > 1.0 && size.height > 1.0 {
        Ok((size.width, size.height))
    } else {
        Err("无法读取悬浮窗尺寸".into())
    }
}

fn work_areas(window: &WebviewWindow) -> Result<Vec<Rect>, String> {
    let monitors = window.available_monitors().map_err(|e| e.to_string())?;
    let areas: Vec<Rect> = monitors.iter().filter_map(work_area).collect();
    if areas.is_empty() {
        return Err("无法获取显示器工作区".into());
    }
    Ok(areas)
}

fn primary_work_area(window: &WebviewWindow) -> Result<Rect, String> {
    let monitor = window
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("无法获取主显示器工作区")?;
    work_area(&monitor).ok_or("无法获取主显示器工作区".into())
}

/// Work area in logical points, converted with the monitor's own scale factor.
/// `None` when the scale would panic inside `to_logical` or the area is unusable.
fn work_area(monitor: &Monitor) -> Option<Rect> {
    let scale = monitor.scale_factor();
    if !(scale.is_sign_positive() && scale.is_normal()) {
        return None;
    }
    let position = monitor.work_area().position.to_logical::<f64>(scale);
    let size = monitor.work_area().size.to_logical::<f64>(scale);
    let rect = Rect { x: position.x, y: position.y, width: size.width, height: size.height };
    usable_area(&rect).then_some(rect)
}

fn intersection(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    Rect { x, y, width: right - x, height: bottom - y }
}

fn finite_rect(rect: Rect) -> bool {
    rect.x.is_finite() && rect.y.is_finite() && rect.width.is_finite() && rect.height.is_finite()
}

fn usable_area(rect: &Rect) -> bool {
    finite_rect(*rect) && rect.width > 0.0 && rect.height > 0.0
}

/// True when the corrected origin is at least one logical pixel away.
fn correction_is_material(current_x: f64, current_y: f64, next_x: f64, next_y: f64) -> bool {
    (next_x - current_x).abs() >= MOVE_DEADBAND || (next_y - current_y).abs() >= MOVE_DEADBAND
}

/// Clamps one axis into the area; an area narrower than the window degenerates
/// to its start edge. Comparisons, not `f64::clamp`, so a non-finite value
/// cannot assert on the main thread.
fn axis(value: f64, start: f64, extent: f64, window_extent: f64) -> f64 {
    if !(value.is_finite() && start.is_finite() && extent.is_finite() && window_extent.is_finite()) {
        return if start.is_finite() { start } else { 0.0 };
    }
    if window_extent >= extent {
        return start;
    }
    let end = start + extent - window_extent;
    if !end.is_finite() || end < start {
        return start;
    }
    if value < start { start } else if value > end { end } else { value }
}

fn distance2(window: Rect, area: Rect) -> f64 {
    let dx = (window.x + window.width / 2.0) - (area.x + area.width / 2.0);
    let dy = (window.y + window.height / 2.0) - (area.y + area.height / 2.0);
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0 };
    const RAIL: (f64, f64) = (368.0, 600.0);
    const RIGHT: f64 = 1920.0 - 368.0;
    const BOTTOM: f64 = 1080.0 - 600.0;

    fn window_at(x: f64, y: f64) -> Rect {
        Rect { x, y, width: RAIL.0, height: RAIL.1 }
    }

    #[test]
    fn a_window_inside_the_work_area_stays_put() {
        assert_eq!(clamp_into_view(window_at(100.0, 100.0), &[AREA]), None);
    }

    #[test]
    fn a_window_past_the_right_edge_is_pulled_back() {
        assert_eq!(clamp_into_view(window_at(1800.0, 100.0), &[AREA]), Some((RIGHT, 100.0)));
    }

    #[test]
    fn a_window_past_the_left_edge_is_pulled_back() {
        assert_eq!(clamp_into_view(window_at(-500.0, 100.0), &[AREA]), Some((0.0, 100.0)));
    }

    #[test]
    fn a_window_above_the_top_edge_is_pulled_down() {
        assert_eq!(clamp_into_view(window_at(100.0, -40.0), &[AREA]), Some((100.0, 0.0)));
    }

    #[test]
    fn a_window_below_the_bottom_edge_is_pulled_up() {
        assert_eq!(clamp_into_view(window_at(100.0, 1000.0), &[AREA]), Some((100.0, BOTTOM)));
    }

    #[test]
    fn a_window_far_outside_returns_into_the_only_work_area() {
        assert_eq!(clamp_into_view(window_at(5000.0, 3000.0), &[AREA]), Some((RIGHT, BOTTOM)));
    }

    #[test]
    fn a_window_on_an_offset_secondary_area_stays_put() {
        let secondary = Rect { x: 1920.0, y: 0.0, width: 2560.0, height: 1440.0 };
        assert_eq!(clamp_into_view(window_at(2100.0, 200.0), &[secondary]), None);
    }

    #[test]
    fn a_window_in_the_gap_between_two_areas_snaps_to_the_closer_one() {
        let left = AREA;
        let right = Rect { x: 2400.0, y: 0.0, width: 1920.0, height: 1080.0 };
        assert_eq!(clamp_into_view(window_at(2032.0, 100.0), &[left, right]), Some((2400.0, 100.0)));
    }

    #[test]
    fn a_window_covered_by_two_adjacent_areas_stays_put() {
        let left = AREA;
        let right = Rect { x: 1920.0, y: 0.0, width: 1920.0, height: 1080.0 };
        assert_eq!(clamp_into_view(window_at(1700.0, 100.0), &[left, right]), None);
    }

    #[test]
    fn an_area_smaller_than_the_window_aligns_to_its_start_edge() {
        let tiny = Rect { x: 0.0, y: 0.0, width: 200.0, height: 300.0 };
        assert_eq!(clamp_into_view(window_at(500.0, 500.0), &[tiny]), Some((0.0, 0.0)));
    }

    #[test]
    fn the_default_position_hugs_the_top_right_corner_of_the_area() {
        let area = Rect { x: 100.0, y: 50.0, width: 1920.0, height: 1080.0 };
        assert_eq!(default_position(area, RAIL.0, RAIL.1), (1640.0, 130.0));
        let (x, y) = default_position(area, RAIL.0, RAIL.1);
        assert!(x + RAIL.0 <= area.x + area.width && y + RAIL.1 <= area.y + area.height);
    }

    #[test]
    fn an_empty_area_list_does_not_move_the_window() {
        assert_eq!(clamp_into_view(window_at(-500.0, -500.0), &[]), None);
    }

    #[test]
    fn non_finite_geometry_does_not_panic_and_is_ignored() {
        let nan = Rect { x: f64::NAN, y: 0.0, width: 100.0, height: 100.0 };
        let infinite = Rect { x: 0.0, y: 0.0, width: f64::INFINITY, height: 100.0 };
        assert_eq!(clamp_into_view(nan, &[AREA]), None);
        assert_eq!(clamp_into_view(window_at(100.0, 100.0), &[nan, infinite]), None);
        assert_eq!(clamp_into_view(window_at(5000.0, 3000.0), &[nan, AREA]), Some((RIGHT, BOTTOM)));
    }

    #[test]
    fn a_zero_size_area_cannot_steal_the_closest_center() {
        let degenerate = Rect { x: 5000.0, y: 3000.0, width: 0.0, height: 0.0 };
        assert_eq!(clamp_into_view(window_at(5000.0, 3000.0), &[degenerate, AREA]), Some((RIGHT, BOTTOM)));
    }

    #[test]
    fn coverage_within_the_epsilon_stays_and_a_larger_sliver_moves() {
        let area = Rect { x: 0.0, y: 0.0, width: 1024.0, height: 1.0 };
        let within = Rect { x: 1.0 / 1024.0, y: 0.0, width: 1024.0, height: 1.0 };
        assert_eq!(clamp_into_view(within, &[area]), None);
        let beyond = Rect { x: 1.0 / 512.0, y: 0.0, width: 1024.0, height: 1.0 };
        assert_eq!(clamp_into_view(beyond, &[area]), Some((0.0, 0.0)));
    }

    #[test]
    fn overlapping_areas_are_counted_twice_so_a_half_covered_window_stays() {
        let area = Rect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 };
        let window = Rect { x: 50.0, y: 0.0, width: 100.0, height: 100.0 };
        assert_eq!(clamp_into_view(window, &[area]), Some((0.0, 0.0)));
        assert_eq!(clamp_into_view(window, &[area, area]), None);
    }

    #[test]
    fn an_aligned_window_larger_than_the_area_still_reports_that_point() {
        let tiny = Rect { x: 0.0, y: 0.0, width: 200.0, height: 300.0 };
        assert_eq!(clamp_into_view(window_at(0.0, 0.0), &[tiny]), Some((0.0, 0.0)));
    }

    #[test]
    fn a_subpixel_correction_is_not_material_and_one_pixel_is() {
        assert!(!correction_is_material(10.0, 10.0, 10.4, 10.0));
        assert!(!correction_is_material(10.0, 20.0, 10.0, 20.0));
        assert!(correction_is_material(10.0, 10.0, 11.0, 10.0));
    }

    #[test]
    fn the_glide_eases_out_from_start_to_target() {
        assert_eq!(glide_point((0.0, 0.0), (200.0, 100.0), 0.0), (0.0, 0.0));
        assert_eq!(glide_point((0.0, 0.0), (200.0, 100.0), 1.0), (200.0, 100.0));
        // Ease-out covers more than half the distance at the time midpoint.
        let (x, y) = glide_point((0.0, 0.0), (200.0, 100.0), 0.5);
        assert!(x > 100.0 && y > 50.0 && x <= 200.0 && y <= 100.0);
        assert_eq!(glide_point((40.0, 50.0), (40.0, 50.0), 0.7), (40.0, 50.0));
    }

    #[test]
    fn a_reset_generation_invalidates_a_prior_correction_snapshot() {
        let seq = AtomicU64::new(0);
        let correction = snapshot(&seq);
        assert!(is_current(&seq, correction));
        let reset = bump(&seq);
        assert!(!is_current(&seq, correction));
        assert!(is_current(&seq, reset));
        let reset_again = bump(&seq);
        assert!(!is_current(&seq, reset));
        assert!(is_current(&seq, reset_again));
    }
}
