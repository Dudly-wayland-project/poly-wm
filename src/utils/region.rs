// Ported from niri (GPL-3.0-or-later): src/utils/region.rs

//! `wl_region` helpers.
//!
//! A protocol region is an ordered list of added and subtracted rectangles.
//! Renderers want the opposite: a short list of disjoint rectangles. Clients
//! that approximate rounded shapes (Noctalia tessellates its panels into
//! one-pixel strips) send dozens of them, so the conversion has to keep every
//! strip instead of collapsing the region into a bounding box, which would paint
//! the corners the client carefully cut away.

use std::cmp::{max, min};
use std::collections::BTreeSet;

use smithay::utils::{Logical, Rectangle};
use smithay::wayland::compositor::{RectangleKind, RegionAttributes};

/// Turn an ordered add/subtract region into disjoint rectangles.
///
/// The region is cut into horizontal bands at every distinct edge; inside a
/// band the spans are merged left to right, so touching strips of the same
/// band become one rectangle. The output is sorted top to bottom, left to
/// right.
pub fn region_to_non_overlapping_rects(region: &RegionAttributes) -> Vec<Rectangle<i32, Logical>> {
    let mut output = Vec::new();

    let ys = BTreeSet::from_iter(
        region
            .rects
            .iter()
            .flat_map(|(_, r)| return [r.loc.y, r.loc.y.saturating_add(r.size.h)]),
    );

    let mut ys = ys.into_iter();
    let Some(mut lo) = ys.next() else {
        return output;
    };

    // Sorted list of non-overlapping [start, end) spans of the current band.
    let mut spans = Vec::<(i32, i32)>::new();

    for hi in ys {
        spans.clear();

        'region: for (kind, r) in &region.rects {
            if hi <= r.loc.y || r.loc.y.saturating_add(r.size.h) <= lo {
                continue;
            }

            let mut x1 = r.loc.x;
            let mut x2 = r.loc.x.saturating_add(r.size.w);
            if x1 >= x2 {
                continue;
            }

            match *kind {
                RectangleKind::Add => {
                    for i in (0..spans.len()).rev() {
                        let (start, end) = spans[i];
                        if end < x1 {
                            spans.insert(i + 1, (x1, x2));
                            continue 'region;
                        }
                        if x2 < start {
                            continue;
                        }
                        spans.remove(i);
                        x1 = min(x1, start);
                        x2 = max(x2, end);
                    }
                    spans.insert(0, (x1, x2));
                }
                RectangleKind::Subtract => {
                    for i in (0..spans.len()).rev() {
                        let (start, end) = spans[i];
                        if end <= x1 {
                            continue 'region;
                        }
                        if x2 <= start {
                            continue;
                        }
                        spans.remove(i);
                        if x2 < end {
                            spans.insert(i, (x2, end));
                        }
                        if start < x1 {
                            spans.insert(i, (start, x1));
                        }
                    }
                }
            }
        }

        for (x1, x2) in spans.drain(..) {
            output.push(Rectangle::from_extremities((x1, lo), (x2, hi)));
        }

        lo = hi;
    }

    // Merge vertically adjacent bands with identical spans, so a plain
    // rectangle with a few strips cut into it stays a handful of pieces.
    let mut merged: Vec<Rectangle<i32, Logical>> = Vec::with_capacity(output.len());
    for rect in output {
        let found = merged.iter_mut().rev().find(|prev| {
            return prev.loc.x == rect.loc.x
                && prev.size.w == rect.size.w
                && prev.loc.y + prev.size.h == rect.loc.y;
        });
        match found {
            Some(prev) => prev.size.h += rect.size.h,
            None => merged.push(rect),
        }
    }
    return merged;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test rectangle: its kind and `(x, y, w, h)`.
    type TestRect = (RectangleKind, (i32, i32, i32, i32));

    fn region(rects: &[TestRect]) -> RegionAttributes {
        return RegionAttributes {
            rects: rects
                .iter()
                .map(|(kind, (x, y, w, h))| {
                    return (*kind, Rectangle::new((*x, *y).into(), (*w, *h).into()));
                })
                .collect(),
        };
    }

    fn area(rects: &[Rectangle<i32, Logical>]) -> i32 {
        return rects.iter().map(|r| return r.size.w * r.size.h).sum();
    }

    fn assert_disjoint(rects: &[Rectangle<i32, Logical>]) {
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.overlaps(*b), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn empty_region_gives_nothing() {
        assert!(region_to_non_overlapping_rects(&region(&[])).is_empty());
    }

    #[test]
    fn overlapping_adds_are_merged() {
        use RectangleKind::Add;
        let rects = region_to_non_overlapping_rects(&region(&[
            (Add, (0, 0, 100, 100)),
            (Add, (50, 50, 100, 100)),
        ]));
        assert_disjoint(&rects);
        assert_eq!(area(&rects), 100 * 100 * 2 - 50 * 50);
    }

    #[test]
    fn subtract_respects_order() {
        use RectangleKind::{Add, Subtract};
        // The hole is cut, then partly filled again by a later add.
        let rects = region_to_non_overlapping_rects(&region(&[
            (Add, (0, 0, 100, 100)),
            (Subtract, (25, 25, 50, 50)),
            (Add, (25, 25, 10, 10)),
        ]));
        assert_disjoint(&rects);
        assert_eq!(area(&rects), 100 * 100 - 50 * 50 + 10 * 10);
    }

    #[test]
    fn tessellated_rounded_rect_keeps_its_corners() {
        use RectangleKind::Add;
        // A 40x40 shape with 1px strips through 4px corner bands, the way
        // Noctalia sends it.
        let insets = [3, 1, 1, 0];
        let mut rects = Vec::new();
        for (row, inset) in insets.iter().enumerate() {
            rects.push((Add, (*inset, row as i32, 40 - inset * 2, 1)));
            rects.push((Add, (*inset, 39 - row as i32, 40 - inset * 2, 1)));
        }
        rects.push((Add, (0, 4, 40, 32)));
        let out = region_to_non_overlapping_rects(&region(&rects));
        assert_disjoint(&out);
        let expected = 40 * 40 - 2 * insets.iter().map(|i| return i * 2).sum::<i32>();
        assert_eq!(area(&out), expected);
        // Equal neighbouring rows merge: 0, 1-2, 3-36, 37-38, 39.
        assert_eq!(out.len(), 5);
        // The top-left pixel stays outside the region.
        assert!(!out.iter().any(|r| return r.contains((0, 0))));
    }
}
