//! GPU readback tests of the OVERLAY icon path end to end: a caller's atlas
//! packed by `pack_icon_atlas`, each feature's quads built by
//! `symbol_pass::overlay_symbol_parts` (the code `MapView::paint` runs per
//! overlay feature), drawn by `IconPipeline` on a real device and read back.
//!
//! Set `GPUI_MAP_TEST_PNG_DIR` to also write each frame, over a light map
//! colour, as a PNG for eyes.

use super::*;
use crate::symbol_pass::{overlay_label_text, overlay_symbol_parts, LabelShapeCache, OverlaySymbolParts};

/// Sprites at 3x, drawn on a 2x display: the consumer's set-up.
const RATIO: f32 = 3.0;
const SCALE: f32 = 2.0;
const SIZE: u32 = 192;

/// A 20x28 pt teardrop pin at 3x: a disc head and a tail down to a tip,
/// with the 6 px margin `sdf_from_mask` asks for.
fn pin() -> image::RgbaImage {
    let (cx, cy, r, tip) = (30.0f32, 30.0f32, 22.0f32, 78.0f32);
    crate::sprite::sdf_from_mask(&image::GrayImage::from_fn(60, 84, |x, y| {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let disc = (fx - cx).hypot(fy - cy) <= r;
        let tail = fy >= cy && fy <= tip && (fx - cx).abs() <= r * (tip - fy) / (tip - cy);
        image::Luma([if disc || tail { 255 } else { 0 }])
    }))
}

/// The consumer's third icon: an 18x18 pt rounded chip at 3x.
fn chip() -> image::RgbaImage {
    crate::sprite::sdf_from_mask(&image::GrayImage::from_fn(54, 54, |x, y| {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let (l, t, r, b, rad) = (6.0f32, 9.0f32, 48.0f32, 45.0f32, 10.0f32);
        let qx = (fx.clamp(l + rad, r - rad) - fx).abs();
        let qy = (fy.clamp(t + rad, b - rad) - fy).abs();
        image::Luma([if fx >= l && fx <= r && fy >= t && fy <= b && qx.hypot(qy) <= rad { 255 } else { 0 }])
    }))
}

/// A plain RGBA corner badge on the pin's own canvas, so it shares the
/// pin's anchor: a red dot top-right, transparent elsewhere.
fn badge() -> image::RgbaImage {
    image::RgbaImage::from_fn(60, 84, |x, y| {
        let d = (x as f32 + 0.5 - 50.0).hypot(y as f32 + 0.5 - 10.0);
        if d <= 7.0 {
            image::Rgba([220, 30, 30, 255])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    })
}

fn chip_geometry() -> crate::sprite::IconGeometry {
    crate::sprite::IconGeometry { stretch_x: vec![(24, 30)], stretch_y: vec![(24, 30)], content: Some([21, 12, 33, 42]) }
}

/// Two SDF pins and the plain badge, and -- with `chip` -- the third SDF
/// icon with stretch geometry, as the consumer's `merge_pins` builds it.
fn sheet(with_chip: bool) -> crate::sprite::SpriteAtlas {
    let mut atlas = crate::sprite::SpriteAtlas::new();
    atlas.insert("stop-pin", pin(), RATIO, true);
    atlas.insert("stop-pin-2", pin(), RATIO, true);
    atlas.insert("corner-badge", badge(), RATIO, false);
    if with_chip {
        atlas.insert("marker-chip", chip(), RATIO, true);
        atlas.set_geometry("marker-chip", chip_geometry());
    }
    atlas
}

fn paint(json: serde_json::Value) -> crate::overlay::FeaturePaint {
    crate::overlay::FeaturePaint::from_properties(json.as_object().expect("an object")).expect("paint")
}

/// Every feature's parts through the real overlay code, then the quads
/// drawn in order on a transparent target and read back.
fn draw(
    atlas: &crate::sprite::SpriteAtlas,
    features: &[(crate::overlay::FeaturePaint, (f32, f32))],
) -> (Vec<u8>, Vec<OverlaySymbolParts>) {
    let mut names: Vec<String> = atlas.names().cloned().collect();
    names.sort();
    let (image, rects) = pack_icon_atlas(atlas, &names);
    let geometry: HashMap<String, crate::sprite::IconGeometry> =
        names.iter().filter_map(|n| Some((n.clone(), atlas.geometry(n)?.clone()))).collect();
    let mut fonts = glyphon::FontSystem::new();
    let mut shapes = LabelShapeCache::new();
    let parts: Vec<OverlaySymbolParts> = features
        .iter()
        .map(|(paint, (x, y))| {
            overlay_symbol_parts(paint, *x, *y, SCALE, &rects, image.dimensions(), &geometry, &mut fonts, &mut shapes)
        })
        .collect();
    let instances: Vec<IconInstance> = parts.iter().flat_map(|p| p.icons.iter().copied()).collect();
    let pixels = pollster::block_on(async {
        let ctx = GpuContext::new_headless().await.expect("headless device");
        let mut pipeline = IconPipeline::new(&ctx, wgpu::TextureFormat::Rgba8Unorm);
        let bind_group = pipeline.upload_atlas(&ctx, &image);
        super::tests::render_icons_to_pixels(&ctx, &mut pipeline, &bind_group, &instances, SIZE).await
    });
    (pixels, parts)
}

fn at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// For eyes: the frame composited over Positron's land colour.
fn dump(pixels: &[u8], name: &str) {
    let Ok(dir) = std::env::var("GPUI_MAP_TEST_PNG_DIR") else { return };
    let land = [242.0f32, 242.0, 239.0];
    let img = image::RgbaImage::from_fn(SIZE, SIZE, |x, y| {
        let p = at(pixels, x, y);
        let a = f32::from(p[3]) / 255.0;
        let c = |i: usize| (f32::from(p[i]) * a + land[i] * (1.0 - a)).round() as u8;
        image::Rgba([c(0), c(1), c(2), 255])
    });
    let path = std::path::Path::new(&dir).join(name);
    img.save(&path).expect("writes the PNG");
    eprintln!("wrote {}", path.display());
}

/// The stop pin: white fill, the route colour as a 1.9 pt halo, anchored
/// by its tip -- and the plain badge on the same anchor.
fn pin_and_badge(at: (f32, f32)) -> Vec<(crate::overlay::FeaturePaint, (f32, f32))> {
    vec![
        (
            paint(serde_json::json!({"icon-image": "stop-pin", "icon-color": "#ffffff",
                "icon-halo-color": "#1d4ed8", "icon-halo-width": 1.9, "icon-anchor": "bottom"})),
            at,
        ),
        (paint(serde_json::json!({"icon-image": "corner-badge", "icon-anchor": "bottom"})), at),
    ]
}

/// Report 1: a translucent rectangle in the halo colour filled a 1.9 pt
/// halo pin's whole quad. At 3x, 1.9 pt is 5.7 of the field's 6 usable
/// texels; the halo's threshold sat within `gamma` of the encoding's floor,
/// so every EMPTY texel of the sprite drew at partial coverage.
///
/// Outside the pin's glyph and halo, every pixel of its quad must be the
/// background. Inside: the white fill, the halo colour just outside the
/// edge, and the badge's own red, all where they belong.
#[test]
fn a_halo_at_the_fields_reach_leaves_the_empty_margin_untouched() {
    // Tip at (96, 150) logical/2 -> the quad is 40x56 physical px whose
    // bottom-centre is the tip: left 76, top 94 in physical pixels.
    let (pixels, parts) = draw(&sheet(true), &pin_and_badge((48.0, 75.0)));
    dump(&pixels, "pin-halo-1.9pt-3x.png");
    let quad = parts[0].icons[0];
    assert_eq!((quad.width, quad.height), (40.0, 56.0), "60x84 texels at 3x on a 2x display");
    let (left, top) = (quad.screen_x as u32, quad.screen_y as u32);
    assert_eq!((left, top), (76, 94));
    // Texel (tx, ty) of the sprite lands at physical pixel left + tx*2/3.
    let px = |tx: f32, ty: f32| at(&pixels, left + (tx * SCALE / RATIO) as u32, top + (ty * SCALE / RATIO) as u32);

    // The empty margin: the quad's top-left corner and both bottom
    // corners beside the tip are more than 6 texels from the shape.
    for (tx, ty) in [(1.0, 1.0), (2.0, 80.0), (58.0, 80.0), (2.0, 60.0), (57.0, 60.0)] {
        let p = px(tx, ty);
        assert!(p[3] <= 2, "texel ({tx}, {ty}) outside the pin and its halo drew {p:?}");
    }
    // Every pixel of the quad's bottom row except around the tip.
    for x in left..left + 40 {
        let p = at(&pixels, x, top + 55);
        if (x as i32 - (left as i32 + 20)).abs() > 6 {
            assert!(p[3] <= 2, "bottom row pixel {x} drew {p:?}");
        }
    }
    // The fill, white, at the disc's centre.
    let fill = px(30.0, 30.0);
    assert!(fill.iter().all(|c| *c > 240), "the fill is white: {fill:?}");
    // The halo, in the route colour, 3 texels above the disc's top edge.
    let halo = px(30.0, 5.0);
    assert!(halo[2] > 180 && halo[0] < 80 && halo[3] > 200, "the halo is the route's blue: {halo:?}");
    // The badge's red dot, drawn as its own pixels on the same anchor.
    let red = px(50.0, 10.0);
    assert!(red[0] > 200 && red[1] < 60 && red[3] > 240, "the badge draws its own red: {red:?}");
}

/// The consumer's question: adding a third SDF icon with stretch geometry
/// repacks the atlas -- can that move or corrupt the pin? It must not:
/// the pin and badge draw byte-identical with and without the chip, so
/// neither the repack, the chip's pixel ratio nor its stretch metadata
/// reaches them.
#[test]
fn adding_a_stretchable_sdf_icon_to_the_atlas_leaves_the_pin_unchanged() {
    let (two, _) = draw(&sheet(false), &pin_and_badge((48.0, 75.0)));
    let (three, _) = draw(&sheet(true), &pin_and_badge((48.0, 75.0)));
    assert!(two.chunks(4).any(|p| p[3] > 0), "the pin drew");
    // Within one level: the repack moves the pin to a different x in the
    // atlas, and the UVs that address it are not exactly representable.
    let worst = two.iter().zip(&three).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
    assert!(worst <= 1, "a third icon in the atlas changed the pin's pixels by up to {worst}");
}

/// Report 2: a chip whose `{point_count_abbreviated}` label resolves to
/// "" on a single, unclustered point. MapLibre draws the icon at its own
/// size with no text and no fit (`symbol_layout.ts`: `shapeText` returns
/// false for empty text, `fitIconToText` runs only with a shaping). So
/// does this, for every `icon-text-fit` -- including `both`, which used to
/// grow the chip to fit an empty line.
#[test]
fn an_unclustered_point_with_an_empty_label_draws_its_icon_at_its_own_size() {
    for fit in ["width", "height", "both"] {
        let chip = |text: &str| {
            paint(serde_json::json!({"icon-image": "marker-chip", "icon-color": "#ffffff",
                "icon-halo-color": "#7c3aed", "icon-halo-width": 1.0, "icon-text-fit": fit,
                "text-field": text}))
        };
        let empty = chip("");
        assert_eq!(overlay_label_text(&empty), None, "an empty label is no label");
        let (pixels, parts) = draw(&sheet(true), &[(empty, (48.0, 48.0))]);
        dump(&pixels, &format!("chip-empty-text-{fit}.png"));
        let p = &parts[0];
        assert!(p.text.is_none() && p.text_box.is_none(), "{fit}: no label and no text box");
        let icon_box = p.icon_box.expect("the icon is placed");
        assert!(crate::collision::symbol_box(p.text_box, p.icon_box, 0.0).is_some(), "{fit}: it is offered to placement");
        // 54 texels at 3x on a 2x display: 36x36 physical px, centred on
        // the anchor (96, 96).
        assert_eq!((icon_box.right - icon_box.left, icon_box.bottom - icon_box.top), (36.0, 36.0), "{fit}: own size");
        let centre = at(&pixels, 96, 96);
        assert!(centre.iter().all(|c| *c > 240), "{fit}: the chip's white fill drew at the anchor: {centre:?}");
        let ring = at(&pixels, 96, 96 - 13);
        assert!(ring[3] > 150 && ring[0] < 200, "{fit}: the chip's halo drew: {ring:?}");

        // The control: a real label still fits the chip around itself.
        let (_, parts) = draw(&sheet(true), &[(chip("1.2k"), (48.0, 48.0))]);
        assert!(parts[0].text.is_some(), "{fit}: a label draws");
    }
}
