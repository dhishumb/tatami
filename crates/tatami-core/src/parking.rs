//! Hiding windows of inactive workspaces by parking them almost entirely
//! off-screen (AeroSpace's trick): macOS keeps a window only if part of it
//! stays on a display, so exactly 1pt is left on its own monitor. The
//! corner is chosen so the rest of the window never lands on a neighbour.

use tatami_protocol::{Rect, Size};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
}

const CORNERS: [Corner; 4] = [
    Corner::BottomRight,
    Corner::BottomLeft,
    Corner::TopRight,
    Corner::TopLeft,
];

/// How much of a window may remain on screen and still count as parked.
const SLIVER: f64 = 2.0;

fn intersection(a: &Rect, b: &Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let r = a.right().min(b.right());
    let bt = a.bottom().min(b.bottom());
    (r > x && bt > y).then(|| Rect::new(x, y, r - x, bt - y))
}

/// Where a window of `size` sits when parked in `corner` of `mon`: the
/// window keeps its size and overlaps the monitor by exactly 1x1pt.
pub fn parking_rect(mon: &Rect, corner: Corner, size: Size) -> Rect {
    let left = mon.x + 1.0 - size.w;
    let top = mon.y + 1.0 - size.h;
    let (x, y) = match corner {
        Corner::BottomRight => (mon.right() - 1.0, mon.bottom() - 1.0),
        Corner::BottomLeft => (left, mon.bottom() - 1.0),
        Corner::TopRight => (mon.right() - 1.0, top),
        Corner::TopLeft => (left, top),
    };
    Rect::new(x, y, size.w, size.h)
}

/// The first corner (BR, BL, TR, TL) whose parking region touches no other
/// monitor. The probe is a parked window as large as the monitor (at least
/// 2000pt), which bounds any window that lives there. With every corner
/// blocked, the least-overlapping one wins.
pub fn parking_corner(mon: &Rect, all: &[Rect]) -> Corner {
    let side = mon.w.max(mon.h).max(2000.0);
    let probe_size = Size { w: side, h: side };
    let overlap = |c: Corner| -> f64 {
        let probe = parking_rect(mon, c, probe_size);
        all.iter()
            .filter(|o| !o.approx_eq(mon, 0.5))
            .filter_map(|o| intersection(&probe, o))
            .map(|i| i.area())
            .sum()
    };
    let mut best = (Corner::BottomRight, f64::INFINITY);
    for c in CORNERS {
        let o = overlap(c);
        if o == 0.0 {
            return c;
        }
        if o < best.1 {
            best = (c, o);
        }
    }
    best.0
}

/// True when `frame` touches some monitor but shows no more than a
/// `SLIVER` of itself on any: our own parked windows, recognised after a
/// restart.
pub fn is_parked(frame: &Rect, mons: &[Rect]) -> bool {
    let mut touches = false;
    for m in mons {
        if let Some(i) = intersection(frame, m) {
            if i.w > SLIVER && i.h > SLIVER {
                return false;
            }
            touches = true;
        }
    }
    touches
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three externals in a row above a built-in panel that sits under the
    /// middle (largest) one.
    const LEFT: Rect = Rect::new(-1688.0, -1080.0, 1920.0, 1080.0);
    const MIDDLE: Rect = Rect::new(232.0, -1440.0, 2560.0, 1440.0);
    const RIGHT: Rect = Rect::new(2792.0, -1080.0, 1920.0, 1080.0);
    const BUILTIN: Rect = Rect::new(0.0, 0.0, 3024.0, 1964.0);
    const ALL: [Rect; 4] = [LEFT, MIDDLE, RIGHT, BUILTIN];

    #[test]
    fn four_display_layout_parks_away_from_every_neighbour() {
        let expect = [
            Corner::BottomLeft,
            Corner::TopRight,
            Corner::BottomRight,
            Corner::BottomRight,
        ];
        for (mon, want) in ALL.iter().zip(expect) {
            let c = parking_corner(mon, &ALL);
            assert_eq!(c, want, "monitor {mon:?}");
            // Biggest window that fits the monitor, and an oversized one.
            for size in [
                mon.size(),
                Size {
                    w: 4000.0,
                    h: 3000.0,
                },
            ] {
                let r = parking_rect(mon, c, size);
                for other in ALL.iter().filter(|o| *o != mon) {
                    assert!(
                        !r.intersects(other),
                        "{c:?} of {mon:?} spills onto {other:?}"
                    );
                }
                assert_eq!(
                    intersection(&r, mon),
                    Some(Rect::new(r_x(mon, c), r_y(mon, c), 1.0, 1.0))
                );
                assert!(is_parked(&r, &ALL));
            }
        }
    }

    fn r_x(m: &Rect, c: Corner) -> f64 {
        if matches!(c, Corner::BottomRight | Corner::TopRight) {
            m.right() - 1.0
        } else {
            m.x
        }
    }

    fn r_y(m: &Rect, c: Corner) -> f64 {
        if matches!(c, Corner::BottomRight | Corner::BottomLeft) {
            m.bottom() - 1.0
        } else {
            m.y
        }
    }

    #[test]
    fn every_corner_leaves_exactly_one_point() {
        let m = Rect::new(100.0, 50.0, 800.0, 600.0);
        let size = Size { w: 300.0, h: 200.0 };
        for c in CORNERS {
            let r = parking_rect(&m, c, size);
            assert_eq!(r.size(), size);
            let i = intersection(&r, &m).unwrap();
            assert_eq!((i.w, i.h), (1.0, 1.0), "{c:?}");
        }
    }

    #[test]
    fn single_monitor_prefers_bottom_right() {
        assert_eq!(parking_corner(&BUILTIN, &[BUILTIN]), Corner::BottomRight);
        assert_eq!(parking_corner(&BUILTIN, &[]), Corner::BottomRight);
    }

    #[test]
    fn side_by_side_equal_monitors_park_on_their_outer_edges() {
        let a = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        let b = Rect::new(1920.0, 0.0, 1920.0, 1080.0);
        // a's bottom-right would leave a 1pt row on b.
        assert_eq!(parking_corner(&a, &[a, b]), Corner::BottomLeft);
        assert_eq!(parking_corner(&b, &[a, b]), Corner::BottomRight);
    }

    #[test]
    fn surrounded_monitor_picks_least_overlap() {
        let c = Rect::new(0.0, 0.0, 1000.0, 1000.0);
        let mut all = vec![c];
        for dx in [-1.0, 0.0, 1.0] {
            for dy in [-1.0, 0.0, 1.0] {
                if dx != 0.0 || dy != 0.0 {
                    all.push(Rect::new(dx * 1000.0, dy * 1000.0, 1000.0, 1000.0));
                }
            }
        }
        // Remove the top-left diagonal neighbour: that corner overlaps least.
        all.retain(|r| *r != Rect::new(-1000.0, -1000.0, 1000.0, 1000.0));
        assert_eq!(parking_corner(&c, &all), Corner::TopLeft);
    }

    #[test]
    fn is_parked_distinguishes_slivers_from_visible_and_lost_windows() {
        let r = parking_rect(&BUILTIN, Corner::BottomRight, Size { w: 800.0, h: 600.0 });
        assert!(is_parked(&r, &ALL));
        assert!(!is_parked(&Rect::new(100.0, 100.0, 800.0, 600.0), &ALL));
        assert!(
            !is_parked(&Rect::new(9000.0, 9000.0, 800.0, 600.0), &ALL),
            "touches nothing"
        );
        // Thin strip down the right edge also counts as parked.
        assert!(is_parked(
            &Rect::new(3023.0, 100.0, 800.0, 600.0),
            &[BUILTIN]
        ));
        // Straddling two monitors with most of it visible on one is not.
        assert!(!is_parked(&Rect::new(-1.0, 100.0, 800.0, 600.0), &ALL));
    }
}
