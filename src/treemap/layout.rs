//! Squarified treemap layout.
//!
//! Pure math: no GPUI types, no rendering. Input is a list of weighted
//! items plus a target rectangle; output is one rectangle per item.
//! Algorithm: Bruls, Huizing & van Wijk, "Squarified Treemaps".

/// Axis-free rectangle used by both the layout and the view layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[allow(dead_code)]
impl FRect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn area(&self) -> f32 {
        self.w * self.h
    }
}

#[derive(Clone, Debug)]
pub struct TreemapItem {
    pub node_id: u32,
    /// Weight in the currently selected metric; must be >= 0.
    pub weight: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TreemapRect {
    pub node_id: u32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Worst aspect ratio among the items of a strip laid out along `side`.
fn row_worst_ratio(areas: &[f64], side: f64, row_area: f64) -> f64 {
    if row_area <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let depth = row_area / side;
    areas
        .iter()
        .map(|&a| {
            let len = a / depth.max(f64::MIN_POSITIVE);
            let r = len / depth;
            if r < 1.0 {
                1.0 / r
            } else {
                r
            }
        })
        .fold(f64::MIN_POSITIVE, f64::max)
}

/// Lay out `items` inside `bounds`. Zero-weight items produce no rectangle.
/// Deterministic: equal weights keep input order.
#[allow(unused_assignments)]
pub fn squarify(items: &[TreemapItem], bounds: FRect) -> Vec<TreemapRect> {
    if bounds.w <= 0.0 || bounds.h <= 0.0 || items.is_empty() {
        return Vec::new();
    }

    // Normalize positive weights into pixel areas.
    let total: f64 = items.iter().map(|i| i.weight.max(0.0)).sum();
    if total <= f64::EPSILON {
        return Vec::new();
    }
    let scale = (bounds.w as f64 * bounds.h as f64) / total;

    let mut ordered: Vec<(f64, u32)> = items
        .iter()
        .filter(|i| i.weight > 0.0)
        .map(|i| (i.weight * scale, i.node_id))
        .collect();
    // Stable sort descending on area so ties stay in input order.
    ordered.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut out: Vec<TreemapRect> = Vec::with_capacity(ordered.len());
    let mut remaining = bounds;

    let mut row_areas: Vec<f64> = Vec::new();
    let mut row_ids: Vec<u32> = Vec::new();
    let mut row_area = 0.0f64;

    macro_rules! flush_row {
        () => {
            if !row_ids.is_empty() {
                // A wide box takes a vertical strip with thickness along x;
                // a tall box takes a horizontal strip with thickness along y.
                let wide = remaining.w >= remaining.h;
                let side = if wide { remaining.h } else { remaining.w } as f64;
                let depth = (row_area / side) as f32;
                let mut offset = if wide { remaining.y } else { remaining.x };
                for (&area, &id) in row_areas.iter().zip(&row_ids) {
                    let len = (area / row_area * side) as f32;
                    let (x, y, w, h) = if wide {
                        (remaining.x, offset, depth, len)
                    } else {
                        (offset, remaining.y, len, depth)
                    };
                    out.push(TreemapRect {
                        node_id: id,
                        x,
                        y,
                        w,
                        h,
                    });
                    offset += len;
                }
                if wide {
                    remaining.x += depth;
                    remaining.w -= depth;
                } else {
                    remaining.y += depth;
                    remaining.h -= depth;
                }
                row_areas.clear();
                row_ids.clear();
                row_area = 0.0;
            }
        };
    }

    for (area, id) in ordered {
        let side = remaining.w.min(remaining.h) as f64;
        let without = row_worst_ratio(&row_areas, side, row_area);
        let mut with = row_areas.clone();
        with.push(area);
        let added = row_worst_ratio(&with, side, row_area + area);

        if row_ids.is_empty() || added <= without {
            row_areas.push(area);
            row_ids.push(id);
            row_area += area;
        } else {
            flush_row!();
            row_areas.push(area);
            row_ids.push(id);
            row_area = area;
        }
    }
    flush_row!();

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(items: &[TreemapItem], w: f32, h: f32) -> Vec<TreemapRect> {
        squarify(items, FRect::new(0.0, 0.0, w, h))
    }

    fn items(weights: &[u64]) -> Vec<TreemapItem> {
        weights
            .iter()
            .enumerate()
            .map(|(i, &w)| TreemapItem {
                node_id: i as u32,
                weight: w as f64,
            })
            .collect()
    }

    fn total_area(rs: &[TreemapRect]) -> f64 {
        rs.iter().map(|r| (r.w * r.h) as f64).sum()
    }

    #[test]
    fn single_item_fills_bounds() {
        let out = rects(&items(&[100]), 400.0, 200.0);
        assert_eq!(out.len(), 1);
        assert!((out[0].w - 400.0).abs() < 0.5);
        assert!((out[0].h - 200.0).abs() < 0.5);
    }

    #[test]
    fn area_is_preserved() {
        let it = items(&[500, 300, 150, 40, 10]);
        let out = rects(&it, 640.0, 480.0);
        assert_eq!(out.len(), 5);
        let bounds = 640.0 * 480.0;
        assert!((total_area(&out) - bounds).abs() < bounds * 0.001);
    }

    #[test]
    fn no_overlap_and_inside_bounds() {
        let it = items(&[90_000, 50_000, 20_000, 8_000, 3_000, 900, 100]);
        let out = rects(&it, 800.0, 600.0);
        assert_eq!(out.len(), 7);
        for (a_ix, a) in out.iter().enumerate() {
            assert!(a.x >= -0.01 && a.y >= -0.01);
            assert!(a.x + a.w <= 800.01 && a.y + a.h <= 600.01, "{a:?}");
            for b in out.iter().skip(a_ix + 1) {
                let overlap_x = a.x.min(a.x + a.w) < b.x.max(b.x + b.w)
                    && b.x.min(b.x + b.w) < a.x.max(a.x + a.w);
                let overlap_y = a.y.min(a.y + a.h) < b.y.max(b.y + b.h)
                    && b.y.min(b.y + b.h) < a.y.max(a.y + a.h);
                assert!(!(overlap_x && overlap_y), "overlap between {a:?} and {b:?}");
            }
        }
    }

    #[test]
    fn larger_weight_larger_area() {
        let it = items(&[1000, 500, 250]);
        let out = rects(&it, 500.0, 500.0);
        let by_id = |id: u32| out.iter().find(|r| r.node_id == id).unwrap();
        assert!((by_id(0).w * by_id(0).h) > (by_id(1).w * by_id(1).h));
        assert!((by_id(1).w * by_id(1).h) > (by_id(2).w * by_id(2).h));
    }

    #[test]
    fn zero_weights_are_skipped() {
        let it = items(&[100, 0, 50]);
        let out = rects(&it, 300.0, 100.0);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.node_id != 1));
    }

    #[test]
    fn extreme_ratios_stay_valid() {
        // 99 GB vs 1 MB vs 1 KB.
        let gb = 99.0 * 1024.0 * 1024.0 * 1024.0;
        let mb = 1024.0 * 1024.0;
        let kb = 1024.0;
        let it = vec![
            TreemapItem {
                node_id: 0,
                weight: gb,
            },
            TreemapItem {
                node_id: 1,
                weight: mb,
            },
            TreemapItem {
                node_id: 2,
                weight: kb,
            },
        ];
        let out = rects(&it, 1000.0, 700.0);
        assert_eq!(out.len(), 3);
        for r in &out {
            assert!(r.w.is_finite() && r.h.is_finite());
            assert!(r.w > 0.0 && r.h > 0.0);
        }
        assert!((total_area(&out) - 700_000.0).abs() < 700.0);
    }

    #[test]
    fn deterministic_layout() {
        let it = items(&[42, 17, 9, 9, 3, 1]);
        assert_eq!(rects(&it, 320.0, 240.0), rects(&it, 320.0, 240.0));
    }

    #[test]
    fn empty_input_empty_output() {
        assert!(squarify(&[], FRect::new(0.0, 0.0, 100.0, 100.0)).is_empty());
    }

    #[test]
    fn degenerate_bounds_no_crash() {
        let it = items(&[10, 5]);
        assert!(rects(&it, 0.0, 100.0).is_empty());
        assert!(rects(&it, 100.0, -5.0).is_empty());
    }
}
