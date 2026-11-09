//! The pure grid solver (§3.8). The overlay layout arrives in phase 2.

use crate::spec::Layout;

/// A widget's rectangle in logical pixels, relative to the page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Logical px between cells.
pub const GUTTER: f32 = 12.0;
/// Below this width every item spans 12 (*compact*).
pub const COMPACT_BELOW: f32 = 640.0;
/// Below this width every span doubles (*medium*).
pub const MEDIUM_BELOW: f32 = 1024.0;

/// Places the grid's items at `width_px` (§3.8 rules 1–6). Items are placed
/// in declaration order; an item that does not fit starts a new row; a row
/// is as tall as its tallest item. A resize re-solves and never refetches.
pub fn solve(layout: &Layout, width_px: f32) -> Vec<(String, Rect)> {
    let Some(items) = &layout.items else { return Vec::new() };
    let row_height = layout.row_height.unwrap_or(100) as f32;
    let col_w = (width_px - GUTTER * 11.0) / 12.0;
    let mut out = Vec::new();
    let (mut cursor, mut row_y, mut row_h) = (1u32, 0.0f32, 0.0f32);
    let mut row_has_items = false;
    for it in items {
        let declared = it.span.unwrap_or(12).clamp(1, 12);
        let span = if width_px < COMPACT_BELOW {
            12
        } else if width_px < MEDIUM_BELOW {
            (declared * 2).min(12)
        } else {
            declared
        };
        let height = it.height.unwrap_or(1).max(1) as f32 * row_height;
        // Compact and medium widths drop the declared column: items flow.
        let wanted = if width_px < MEDIUM_BELOW { None } else { it.col };
        let fits_at = |c: u32| c >= cursor && c + span - 1 <= 12;
        let col = match wanted {
            Some(c) if fits_at(c.max(1)) => c.max(1),
            Some(c) => {
                if row_has_items {
                    row_y += row_h + GUTTER;
                    row_h = 0.0;
                }
                c.max(1).min(13 - span)
            }
            None if fits_at(cursor) => cursor,
            None => {
                if row_has_items {
                    row_y += row_h + GUTTER;
                    row_h = 0.0;
                }
                1
            }
        };
        let x = (col - 1) as f32 * (col_w + GUTTER);
        let w = span as f32 * col_w + (span - 1) as f32 * GUTTER;
        out.push((it.widget.clone(), Rect { x, y: row_y, w, h: height }));
        row_h = row_h.max(height);
        cursor = col + span;
        row_has_items = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::GridItem;

    fn item(w: &str, col: Option<u32>, span: u32, height: u32) -> GridItem {
        GridItem { widget: w.into(), col, span: Some(span), height: Some(height) }
    }

    fn grid(items: Vec<GridItem>) -> Layout {
        Layout { columns: Some(12), row_height: Some(100), items: Some(items), overlay: None }
    }

    #[test]
    fn items_flow_in_declaration_order() {
        let l = grid(vec![item("a", None, 4, 1), item("b", None, 4, 2), item("c", None, 4, 1)]);
        let r = solve(&l, 1280.0);
        assert!(r[0].1.x < r[1].1.x && r[1].1.x < r[2].1.x);
        assert!(r.iter().all(|(_, rect)| rect.y == 0.0));
    }

    #[test]
    fn an_item_that_does_not_fit_starts_a_row() {
        let l = grid(vec![item("a", Some(1), 6, 3), item("b", Some(7), 6, 1), item("c", None, 4, 1), item("d", Some(3), 2, 1)]);
        let r = solve(&l, 1280.0);
        assert_eq!(r[1].1.y, 0.0);
        assert_eq!(r[2].1.y, 300.0 + GUTTER, "the row is as tall as its tallest item");
        assert_eq!(r[2].1.x, 0.0);
        // `d` asks for column 3, behind the cursor (5): a new row at column 3.
        assert_eq!(r[3].1.y, r[2].1.y + 100.0 + GUTTER);
        assert!(r[3].1.x > 0.0);
    }

    #[test]
    fn compact_width_spans_twelve() {
        let l = grid(vec![item("a", Some(1), 6, 3), item("b", Some(7), 6, 1)]);
        let r = solve(&l, 390.0);
        assert_eq!(r[0].1.w, 390.0);
        assert_eq!(r[1].1.y, 300.0 + GUTTER);
        assert_eq!(r[1].1.h, 100.0, "keeps its declared height");
    }

    #[test]
    fn medium_width_doubles_spans() {
        let l = grid(vec![item("a", None, 3, 1), item("b", None, 3, 1), item("c", None, 6, 1)]);
        let r = solve(&l, 800.0);
        let col = (800.0 - GUTTER * 11.0) / 12.0;
        assert_eq!(r[0].1.w, 6.0 * col + 5.0 * GUTTER);
        assert_eq!(r[1].1.y, 0.0);
        assert_eq!(r[2].1.w, 800.0, "6 doubles to 12");
        assert!(r[2].1.y > 0.0);
    }

    #[test]
    fn a_resize_from_1280_to_390_moves_rectangles() {
        let l = grid(vec![item("a", Some(1), 6, 3), item("b", Some(7), 6, 3)]);
        assert_ne!(solve(&l, 1280.0), solve(&l, 390.0));
    }
}
