//! Tray status icons: the app logo with a small status badge in the
//! bottom-right corner, drawn from scratch (no image crate) so the tray can
//! show sync state at a glance - synced, syncing, paused, error, signed out,
//! waiting for the keyring.
//!
//! The badge pairs a color with a distinct white glyph (check, dots, bars,
//! "!", dash, clock) so the state doesn't hinge on telling hues apart.

use crate::service::OverallStatus;

/// Side length, in pixels, of the (square) tray icons.
pub const ICON_SIZE: usize = 64;

const SIZE_F: f32 = ICON_SIZE as f32;
/// Badge fill radius, and the white ring drawn around it for contrast on both
/// light and dark panels.
const BADGE_R: f32 = 14.0;
const RING: f32 = 2.0;
/// Badge center: tucked into the bottom-right corner, ring inside the canvas.
const BADGE_C: f32 = SIZE_F - BADGE_R - RING - 1.0;

const WHITE: [u8; 3] = [255, 255, 255];

/// The badge fill color for `status`.
pub fn status_color(status: OverallStatus) -> [u8; 3] {
    match status {
        OverallStatus::Synced => [0x34, 0xC7, 0x59],
        OverallStatus::Syncing => [0x3B, 0x9C, 0xFF],
        OverallStatus::Paused => [0x8E, 0x8B, 0x9A],
        OverallStatus::Error => [0xE0, 0x50, 0x64],
        // Amber for both "needs you, and nothing is wrong with the files": the
        // glyph says which.
        OverallStatus::SignedOut | OverallStatus::WaitingForKeyring => [0xF0, 0xA0, 0x30],
    }
}

/// Every status, for building the full set of icons up front.
pub const ALL_STATUSES: [OverallStatus; 6] = [
    OverallStatus::Synced,
    OverallStatus::Syncing,
    OverallStatus::Paused,
    OverallStatus::Error,
    OverallStatus::SignedOut,
    OverallStatus::WaitingForKeyring,
];

/// An `ICON_SIZE` x `ICON_SIZE` RGBA icon: `logo` (RGBA, `src_w` x `src_h`,
/// already shrunk to fit within `ICON_SIZE` - anything larger is cropped)
/// centered on a transparent square canvas, with `status`'s badge on top.
pub fn status_icon_rgba(logo: &[u8], src_w: usize, src_h: usize, status: OverallStatus) -> Vec<u8> {
    let mut out = vec![0u8; ICON_SIZE * ICON_SIZE * 4];

    let (cw, ch) = (src_w.min(ICON_SIZE), src_h.min(ICON_SIZE));
    let (ox, oy) = ((ICON_SIZE - cw) / 2, (ICON_SIZE - ch) / 2);
    for y in 0..ch {
        for x in 0..cw {
            let s = (y * src_w + x) * 4;
            let d = ((oy + y) * ICON_SIZE + ox + x) * 4;
            if let (Some(src), Some(dst)) = (logo.get(s..s + 4), out.get_mut(d..d + 4)) {
                dst.copy_from_slice(src);
            }
        }
    }

    let c = BADGE_C;
    let u = BADGE_R;
    paint_disc(&mut out, c, c, BADGE_R + RING, WHITE);
    paint_disc(&mut out, c, c, BADGE_R, status_color(status));
    match status {
        OverallStatus::Synced => {
            // check mark
            paint_segment(
                &mut out,
                (c - 0.45 * u, c + 0.02 * u),
                (c - 0.12 * u, c + 0.36 * u),
                1.3,
                WHITE,
            );
            paint_segment(
                &mut out,
                (c - 0.12 * u, c + 0.36 * u),
                (c + 0.50 * u, c - 0.34 * u),
                1.3,
                WHITE,
            );
        }
        OverallStatus::Syncing => {
            // "working..." ellipsis
            for dx in [-0.46, 0.0, 0.46] {
                paint_disc(&mut out, c + dx * u, c, 1.9, WHITE);
            }
        }
        OverallStatus::Paused => {
            // pause bars
            for dx in [-0.27, 0.27] {
                paint_segment(
                    &mut out,
                    (c + dx * u, c - 0.42 * u),
                    (c + dx * u, c + 0.42 * u),
                    1.5,
                    WHITE,
                );
            }
        }
        OverallStatus::Error => {
            // exclamation mark
            paint_segment(&mut out, (c, c - 0.52 * u), (c, c + 0.10 * u), 1.5, WHITE);
            paint_disc(&mut out, c, c + 0.50 * u, 1.7, WHITE);
        }
        OverallStatus::SignedOut => {
            // "blocked" dash
            paint_segment(&mut out, (c - 0.48 * u, c), (c + 0.48 * u, c), 1.6, WHITE);
        }
        OverallStatus::WaitingForKeyring => {
            // a small clock: a ring with two hands
            let r = 0.56 * u;
            paint_disc(&mut out, c, c, r + 1.1, WHITE);
            paint_disc(&mut out, c, c, r - 1.1, status_color(status));
            paint_segment(&mut out, (c, c), (c, c - 0.34 * u), 1.0, WHITE);
            paint_segment(&mut out, (c, c), (c + 0.26 * u, c), 1.0, WHITE);
        }
    }
    out
}

/// Composite `rgb` at `coverage` (0..=1) over the straight-alpha RGBA pixel `px`.
fn over(px: &mut [u8], rgb: [u8; 3], coverage: f32) {
    let sa = coverage.clamp(0.0, 1.0);
    if sa <= 0.0 {
        return;
    }
    let da = px[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    for i in 0..3 {
        let sc = rgb[i] as f32;
        let dc = px[i] as f32;
        px[i] = ((sc * sa + dc * da * (1.0 - sa)) / oa).round() as u8;
    }
    px[3] = (oa * 255.0).round() as u8;
}

/// Anti-aliased filled disc: coverage falls off over the last pixel of its edge.
fn paint_disc(buf: &mut [u8], cx: f32, cy: f32, r: f32, rgb: [u8; 3]) {
    paint_with(buf, cx - r, cy - r, cx + r, cy + r, rgb, |px, py| {
        let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
        r + 0.5 - d
    });
}

/// Anti-aliased line segment with round caps, `half_w` pixels either side.
fn paint_segment(buf: &mut [u8], a: (f32, f32), b: (f32, f32), half_w: f32, rgb: [u8; 3]) {
    let pad = half_w + 1.0;
    paint_with(
        buf,
        a.0.min(b.0) - pad,
        a.1.min(b.1) - pad,
        a.0.max(b.0) + pad,
        a.1.max(b.1) + pad,
        rgb,
        |px, py| {
            let (abx, aby) = (b.0 - a.0, b.1 - a.1);
            let len2 = abx * abx + aby * aby;
            let t = if len2 == 0.0 {
                0.0
            } else {
                (((px - a.0) * abx + (py - a.1) * aby) / len2).clamp(0.0, 1.0)
            };
            let (nx, ny) = (a.0 + t * abx, a.1 + t * aby);
            let d = ((px - nx).powi(2) + (py - ny).powi(2)).sqrt();
            half_w + 0.5 - d
        },
    );
}

/// Visit every pixel in the bounding box (clamped to the canvas), asking
/// `signed_cov` for the coverage at the pixel's center, and composite `rgb`.
fn paint_with(
    buf: &mut [u8],
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rgb: [u8; 3],
    signed_cov: impl Fn(f32, f32) -> f32,
) {
    let lo = |v: f32| v.floor().max(0.0) as usize;
    let hi = |v: f32| (v.ceil().max(0.0) as usize).min(ICON_SIZE - 1);
    for y in lo(y0)..=hi(y1) {
        for x in lo(x0)..=hi(x1) {
            let cov = signed_cov(x as f32 + 0.5, y as f32 + 0.5);
            let i = (y * ICON_SIZE + x) * 4;
            over(&mut buf[i..i + 4], rgb, cov);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: f32 = BADGE_R;

    fn px(buf: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * ICON_SIZE + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    /// An opaque solid-color "logo" of the given size.
    fn solid(w: usize, h: usize, rgba: [u8; 4]) -> Vec<u8> {
        rgba.repeat(w * h)
    }

    fn badge_px(buf: &[u8], dx: f32, dy: f32) -> [u8; 4] {
        px(buf, (BADGE_C + dx) as usize, (BADGE_C + dy) as usize)
    }

    #[test]
    fn output_is_a_square_rgba_canvas() {
        let logo = solid(10, 10, [1, 2, 3, 255]);
        let out = status_icon_rgba(&logo, 10, 10, OverallStatus::Synced);
        assert_eq!(out.len(), ICON_SIZE * ICON_SIZE * 4);
    }

    #[test]
    fn a_non_square_logo_is_centered_on_the_canvas() {
        // 64 wide x 40 tall: 12 transparent rows above and below.
        let logo = solid(ICON_SIZE, 40, [200, 10, 10, 255]);
        let out = status_icon_rgba(&logo, ICON_SIZE, 40, OverallStatus::Paused);
        assert_eq!(px(&out, 2, 0)[3], 0, "row above the logo stays transparent");
        assert_eq!(px(&out, 2, 11)[3], 0);
        assert_eq!(px(&out, 2, 12), [200, 10, 10, 255], "first logo row");
        assert_eq!(px(&out, 2, 51), [200, 10, 10, 255], "last logo row");
        assert_eq!(px(&out, 2, 52)[3], 0);
    }

    #[test]
    fn logo_pixels_away_from_the_badge_are_untouched() {
        let logo = solid(ICON_SIZE, ICON_SIZE, [9, 8, 7, 255]);
        for status in ALL_STATUSES {
            let out = status_icon_rgba(&logo, ICON_SIZE, ICON_SIZE, status);
            assert_eq!(px(&out, 5, 5), [9, 8, 7, 255]);
            assert_eq!(px(&out, 30, 10), [9, 8, 7, 255]);
        }
    }

    #[test]
    fn badge_fill_is_the_status_color() {
        // Up-left of the badge center, clear of every glyph.
        let logo = solid(ICON_SIZE, ICON_SIZE, [0, 0, 0, 255]);
        for status in ALL_STATUSES {
            let out = status_icon_rgba(&logo, ICON_SIZE, ICON_SIZE, status);
            let c = status_color(status);
            assert_eq!(
                badge_px(&out, -0.57 * U, -0.57 * U),
                [c[0], c[1], c[2], 255],
                "{status:?}"
            );
        }
    }

    #[test]
    fn badge_has_a_white_ring_just_outside_the_fill() {
        let logo = solid(ICON_SIZE, ICON_SIZE, [0, 0, 0, 255]);
        let out = status_icon_rgba(&logo, ICON_SIZE, ICON_SIZE, OverallStatus::Error);
        // Straight left of the center, past the fill radius, inside the ring.
        assert_eq!(badge_px(&out, -(U + 1.0), 0.0), [255, 255, 255, 255]);
    }

    #[test]
    fn each_status_draws_its_own_glyph() {
        let logo = solid(ICON_SIZE, ICON_SIZE, [0, 0, 0, 255]);
        let white = [255, 255, 255, 255];
        let render = |s| status_icon_rgba(&logo, ICON_SIZE, ICON_SIZE, s);

        // Paused: two bars either side of center, nothing at the very center.
        let paused = render(OverallStatus::Paused);
        assert_eq!(badge_px(&paused, -0.27 * U, 0.0), white);
        assert_eq!(badge_px(&paused, 0.27 * U, 0.0), white);
        assert_ne!(badge_px(&paused, 0.0, 0.0), white);

        // Error: the "!" bar sits above center, its dot below.
        let error = render(OverallStatus::Error);
        assert_eq!(badge_px(&error, 0.0, -0.2 * U), white);
        assert_eq!(badge_px(&error, 0.0, 0.5 * U), white);

        // Signed out: a horizontal dash through the center.
        let out = render(OverallStatus::SignedOut);
        assert_eq!(badge_px(&out, 0.0, 0.0), white);
        assert_eq!(badge_px(&out, 0.4 * U, 0.0), white);

        // Waiting for the keyring: a clock - a ring, a hand up and a hand right.
        let waiting = render(OverallStatus::WaitingForKeyring);
        assert_eq!(badge_px(&waiting, 0.56 * U, 0.0), white, "ring, right");
        assert_eq!(badge_px(&waiting, -0.56 * U, 0.0), white, "ring, left");
        assert_eq!(badge_px(&waiting, 0.0, -0.25 * U), white, "hand up");
        assert_eq!(badge_px(&waiting, 0.18 * U, 0.0), white, "hand right");
        assert_ne!(
            badge_px(&waiting, -0.3 * U, 0.0),
            white,
            "no hand to the left"
        );
        assert_ne!(badge_px(&waiting, 0.0, 0.3 * U), white, "no hand down");

        // Syncing: three dots in a row.
        let syncing = render(OverallStatus::Syncing);
        assert_eq!(badge_px(&syncing, -0.46 * U, 0.0), white);
        assert_eq!(badge_px(&syncing, 0.0, 0.0), white);
        assert_eq!(badge_px(&syncing, 0.46 * U, 0.0), white);

        // Synced: a check - white on its lower-left stroke, not at top-left.
        let synced = render(OverallStatus::Synced);
        assert_eq!(badge_px(&synced, -0.28 * U, 0.2 * U), white);
        assert_ne!(badge_px(&synced, -0.4 * U, -0.4 * U), white);
    }

    #[test]
    fn every_status_renders_a_different_icon() {
        let logo = solid(ICON_SIZE, ICON_SIZE, [0, 0, 0, 255]);
        let icons: Vec<Vec<u8>> = ALL_STATUSES
            .iter()
            .map(|&s| status_icon_rgba(&logo, ICON_SIZE, ICON_SIZE, s))
            .collect();
        for i in 0..icons.len() {
            for j in (i + 1)..icons.len() {
                assert_ne!(icons[i], icons[j], "statuses {i} and {j} look identical");
            }
        }
    }

    #[test]
    fn a_transparent_logo_still_gets_a_visible_badge() {
        let logo = solid(ICON_SIZE, ICON_SIZE, [0, 0, 0, 0]);
        let out = status_icon_rgba(&logo, ICON_SIZE, ICON_SIZE, OverallStatus::Synced);
        assert_eq!(px(&out, 3, 3)[3], 0, "empty corner stays transparent");
        assert_eq!(badge_px(&out, -0.57 * U, -0.57 * U)[3], 255);
    }

    #[test]
    fn an_oversized_logo_is_cropped_not_a_panic() {
        let logo = solid(200, 100, [5, 5, 5, 255]);
        let out = status_icon_rgba(&logo, 200, 100, OverallStatus::Synced);
        assert_eq!(out.len(), ICON_SIZE * ICON_SIZE * 4);
    }
}
