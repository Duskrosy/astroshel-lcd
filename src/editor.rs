// Editor geometry helpers: hit-testing, move, and resize operations for freeform dashboard customization.
// Pure logic, no rendering or I/O.

use crate::dashboard::{Rect, Widget, CANVAS};

pub const MIN_W: u32 = 24;
pub const MIN_H: u32 = 24;
pub const HANDLE: f32 = 12.0;

/// Grab point on a widget rect: which part is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grab {
    Body,
    TL,
    TR,
    BL,
    BR,
}

/// Hit-test: find the topmost (last-drawn, last in vec) widget whose rect contains (x,y).
pub fn hit_widget(widgets: &[Widget], x: u32, y: u32) -> Option<usize> {
    for (i, w) in widgets.iter().enumerate().rev() {
        let r = &w.rect;
        if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
            return Some(i);
        }
    }
    None
}

/// Hit-test a rect's handles and body.
/// If (x,y) is within HANDLE px of a corner → that corner;
/// else if inside rect → Body;
/// else None.
pub fn hit_handle(rect: &Rect, x: f32, y: f32) -> Option<Grab> {
    let x0 = rect.x as f32;
    let y0 = rect.y as f32;
    let x1 = (rect.x + rect.w) as f32;
    let y1 = (rect.y + rect.h) as f32;

    // Check corners (within HANDLE px) first
    if (x - x0).abs() <= HANDLE && (y - y0).abs() <= HANDLE {
        return Some(Grab::TL);
    }
    if (x - x1).abs() <= HANDLE && (y - y0).abs() <= HANDLE {
        return Some(Grab::TR);
    }
    if (x - x0).abs() <= HANDLE && (y - y1).abs() <= HANDLE {
        return Some(Grab::BL);
    }
    if (x - x1).abs() <= HANDLE && (y - y1).abs() <= HANDLE {
        return Some(Grab::BR);
    }

    // Check body: inside rect (inclusive on min, exclusive on max)
    if x >= x0 && x < x1 && y >= y0 && y < y1 {
        return Some(Grab::Body);
    }

    None
}

/// Translate a rect by (dx, dy), clamped so it stays fully within 0..CANVAS.
pub fn apply_move(rect: Rect, dx: i32, dy: i32) -> Rect {
    // Use i64 intermediate to avoid overflow during arithmetic
    let new_x = (rect.x as i64) + (dx as i64);
    let new_y = (rect.y as i64) + (dy as i64);

    // Clamp so the rect stays within canvas bounds
    let clamped_x = new_x.max(0).min((CANVAS as i64) - (rect.w as i64)) as u32;
    let clamped_y = new_y.max(0).min((CANVAS as i64) - (rect.h as i64)) as u32;

    Rect {
        x: clamped_x,
        y: clamped_y,
        w: rect.w,
        h: rect.h,
    }
}

/// Resize a rect by dragging a corner, enforcing MIN_W/MIN_H and canvas bounds.
pub fn apply_resize(rect: Rect, grab: Grab, dx: i32, dy: i32) -> Rect {
    // Use i64 for intermediate arithmetic to handle large deltas safely
    let mut new_x = rect.x as i64;
    let mut new_y = rect.y as i64;
    let mut new_w = rect.w as i64;
    let mut new_h = rect.h as i64;

    match grab {
        Grab::TL => {
            // Moving top-left corner: adjust x, y, w, h
            new_x = new_x + (dx as i64);
            new_y = new_y + (dy as i64);
            new_w = new_w - (dx as i64);
            new_h = new_h - (dy as i64);
        }
        Grab::TR => {
            // Moving top-right corner: adjust y, w, h (not x)
            new_y = new_y + (dy as i64);
            new_w = new_w + (dx as i64);
            new_h = new_h - (dy as i64);
        }
        Grab::BL => {
            // Moving bottom-left corner: adjust x, w, h (not y)
            new_x = new_x + (dx as i64);
            new_w = new_w - (dx as i64);
            new_h = new_h + (dy as i64);
        }
        Grab::BR => {
            // Moving bottom-right corner: adjust w, h (not x or y)
            new_w = new_w + (dx as i64);
            new_h = new_h + (dy as i64);
        }
        Grab::Body => {
            // Body dragging handled by apply_move, not here
        }
    }

    // Enforce minimum size
    new_w = new_w.max(MIN_W as i64);
    new_h = new_h.max(MIN_H as i64);

    // Clamp width/height to fit within canvas
    new_w = new_w.min(CANVAS as i64);
    new_h = new_h.min(CANVAS as i64);

    // Clamp position so the rect stays within canvas (and respects width/height)
    new_x = new_x.max(0).min((CANVAS as i64) - new_w);
    new_y = new_y.max(0).min((CANVAS as i64) - new_h);

    Rect {
        x: new_x as u32,
        y: new_y as u32,
        w: new_w as u32,
        h: new_h as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::WidgetKind;

    fn wr(x: u32, y: u32, w: u32, h: u32) -> Widget {
        let mut wi = Widget::new(WidgetKind::Text);
        wi.rect = Rect { x, y, w, h };
        wi
    }

    #[test]
    fn hit_widget_returns_topmost() {
        let ws = vec![wr(0, 0, 100, 100), wr(20, 20, 100, 100)];
        assert_eq!(hit_widget(&ws, 30, 30), Some(1)); // overlapping → last wins
        assert_eq!(hit_widget(&ws, 5, 5), Some(0));
        assert_eq!(hit_widget(&ws, 300, 300), None);
    }

    #[test]
    fn hit_handle_detects_corner_vs_body() {
        let r = Rect {
            x: 50,
            y: 50,
            w: 100,
            h: 100,
        };
        assert!(matches!(hit_handle(&r, 51.0, 51.0), Some(Grab::TL)));
        assert!(matches!(hit_handle(&r, 149.0, 149.0), Some(Grab::BR)));
        assert!(matches!(hit_handle(&r, 100.0, 100.0), Some(Grab::Body)));
        assert!(hit_handle(&r, 5.0, 5.0).is_none());
    }

    #[test]
    fn move_clamps_within_canvas() {
        let r = Rect {
            x: 10,
            y: 10,
            w: 100,
            h: 100,
        };
        let m = apply_move(r, -50, -50);
        assert_eq!((m.x, m.y), (0, 0)); // clamped at origin
        let m2 = apply_move(r, 1000, 1000);
        assert_eq!(m2.x + m2.w, CANVAS); // clamped at far edge
        assert_eq!(m2.w, 100); // size preserved
    }

    #[test]
    fn resize_enforces_min_and_bounds() {
        let r = Rect {
            x: 50,
            y: 50,
            w: 100,
            h: 100,
        };
        let small = apply_resize(r, Grab::BR, -1000, -1000);
        assert!(small.w >= MIN_W && small.h >= MIN_H);
        let big = apply_resize(r, Grab::BR, 1000, 1000);
        assert!(big.x + big.w <= CANVAS && big.y + big.h <= CANVAS);
    }
}
