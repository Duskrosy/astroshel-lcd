//! Rasterizes the brand mark (`assets/logo.svg`) into RGBA8888 buffers used
//! for both the tray icon and the window/taskbar icon.
//!
//! `resvg`/`usvg` 0.47 render into a `tiny_skia::Pixmap`, whose backing bytes
//! are *premultiplied* alpha (see `tiny_skia::Pixmap` docs: "A container that
//! owns premultiplied RGBA pixels"). Both `tray_icon::Icon::from_rgba` and
//! `egui::IconData` expect straight (unassociated) alpha, so every pixel is
//! unpremultiplied before being handed back.

const LOGO_SVG: &[u8] = include_bytes!("../assets/logo.svg");

/// Renders the brand SVG into a `size` x `size` straight-alpha RGBA8888
/// buffer. Returns `None` on any parse/render failure (corrupt asset, zero
/// size, allocation failure, ...) -- callers must fall back gracefully and
/// must never panic here.
pub fn load_icon_rgba(size: u32) -> Option<(Vec<u8>, u32, u32)> {
    if size == 0 {
        return None;
    }

    // `resvg` re-exports its exact `usvg`/`tiny_skia` versions (`resvg::usvg`,
    // `resvg::tiny_skia`); using those re-exports instead of separate `usvg`/
    // `tiny-skia` dependencies avoids any risk of a version mismatch between them.
    let opts = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(LOGO_SVG, &opts).ok()?;

    let tree_size = tree.size();
    let (sw, sh) = (tree_size.width(), tree_size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }

    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;

    // Uniform scale-to-fit + center, in case the target isn't square-fitting
    // the source viewBox exactly (logo.svg is 200x200, so this is a no-op
    // scale for square targets, but keeps the helper correct in general).
    let scale = (size as f32 / sw).min(size as f32 / sh);
    let tx = (size as f32 - sw * scale) / 2.0;
    let ty = (size as f32 - sh * scale) / 2.0;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let mut rgba = pixmap.take();
    unpremultiply_in_place(&mut rgba);
    Some((rgba, size, size))
}

/// Converts a buffer of premultiplied RGBA8888 pixels to straight alpha, in place.
fn unpremultiply_in_place(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        // a == 0: pixel is fully transparent and tiny-skia already zeroes
        // r/g/b for it, so there's nothing to unpremultiply (and dividing by
        // a zero alpha would be nonsensical anyway).
        // a == 255: premultiplied == straight, no-op.
        if a != 0 && a != 255 {
            if let Some(pm) = tiny_skia::PremultipliedColorU8::from_rgba(px[0], px[1], px[2], a) {
                let straight = pm.demultiply();
                px[0] = straight.red();
                px[1] = straight.green();
                px[2] = straight.blue();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_at_requested_size() {
        let (rgba, w, h) = load_icon_rgba(64).expect("logo.svg should rasterize");
        assert_eq!(w, 64);
        assert_eq!(h, 64);
        assert_eq!(rgba.len(), (64 * 64 * 4) as usize);
    }

    #[test]
    fn zero_size_returns_none() {
        assert!(load_icon_rgba(0).is_none());
    }

    #[test]
    fn produces_some_opaque_and_some_transparent_pixels() {
        // logo.svg has an opaque circular disc on a fully transparent
        // background, so a render should contain both fully-transparent
        // corner pixels and fully-opaque center pixels.
        let (rgba, w, h) = load_icon_rgba(64).unwrap();
        let corner = &rgba[0..4];
        assert_eq!(corner[3], 0, "corner pixel should be fully transparent");

        let cx = (w / 2) as usize;
        let cy = (h / 2) as usize;
        let idx = (cy * w as usize + cx) * 4;
        let center = &rgba[idx..idx + 4];
        assert_eq!(center[3], 255, "center pixel should be fully opaque");
    }
}
