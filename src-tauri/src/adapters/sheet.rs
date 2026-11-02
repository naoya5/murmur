//! Adapter: the pictures of an annotation bundle. Draws each keyframe with its marks
//! (`frames/<n>.png`) and the contact sheet of all keyframes (`contact-sheet.png`) with
//! CoreGraphics, CoreText and ImageIO, through the shared `image` helpers.
//!
//! The layout rules (grid by count, gutters, chip size) live here with pure helpers that tests
//! cover; the drawing itself is smoke-tested with synthetic pictures. Pictures and the words
//! they go with never reach the log (G7). Nothing here panics (G6).

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use murmur_core::annotations::{Mark, Point, ink_label};
use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CFType, CGFloat, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{CGColor, CGContext, CGLineCap, CGLineJoin, CGPath};
use objc2_core_text::{CTFont, CTLine, kCTFontAttributeName, kCTForegroundColorAttributeName};

use super::image::{self, Bitmap, Canvas, ImageError};

/// Long edge of the contact sheet and of each frame (the size the screen adapter captures).
pub const LONG_EDGE_PX: f64 = 1568.0;
/// White space between the tiles of the sheet and around them.
pub const GUTTER_PX: f64 = 8.0;
/// Height of the `#n` chip text on the sheet (smaller was not readable to the models).
pub const CHIP_PX: f64 = 24.0;
/// Colour of an ink stroke: `--ink-stroke` in `src/styles.css` (a test keeps them equal).
pub const INK_STROKE_RGB: (f64, f64, f64) = (1.0, 59.0 / 255.0, 48.0 / 255.0);
/// Outer edge of a stroke, so it reads on dark screens too: `--ink-stroke-edge`.
pub const INK_EDGE_RGBA: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.92);
/// Stroke widths in pixels on a frame; the sheet scales them with its tiles.
const INK_EDGE_WIDTH: f64 = 7.0;
const INK_CORE_WIDTH: f64 = 4.0;
const CLICK_RADIUS: f64 = 14.0;
const DWELL_RADIUS: f64 = 16.0;
/// Mode of the bundle folders.
const FOLDER_MODE: u32 = 0o700;

#[derive(Debug, thiserror::Error)]
pub enum SheetError {
    #[error("no keyframe to draw")]
    Empty,
    #[error("too many keyframes: {0}")]
    TooMany(usize),
    #[error(transparent)]
    Image(#[from] ImageError),
    #[error("could not create {0}")]
    Folder(String),
}

/// One keyframe to draw: its picture and the marks assigned to it.
pub struct FrameInput<'a> {
    /// The picture number, from 1.
    pub n: usize,
    pub picture: &'a Bitmap,
    pub marks: &'a [Mark],
}

/// What `render` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub sheet: PathBuf,
    /// `frames/1.png` .. in keyframe order.
    pub frames: Vec<PathBuf>,
    pub sheet_size: (usize, usize),
    pub draw_ms: u64,
    pub encode_ms: u64,
}

/// A rectangle in pixels with the top-left origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Columns and rows of the sheet for `n` pictures (1: 1x1, 2: 2x1, 3-4: 2x2, 5-6: 3x2).
pub fn grid_for(n: usize) -> (usize, usize) {
    match n {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        _ => (3, 2),
    }
}

/// The sheet size and the tile of each picture, for pictures of aspect `height / width`.
/// The sheet is `LONG_EDGE_PX` wide; tiles fill it with `GUTTER_PX` between and around them.
pub fn sheet_layout(n: usize, aspect: f64) -> (usize, usize, Vec<Tile>) {
    let (cols, rows) = grid_for(n);
    let (cols_f, rows_f) = (cols as f64, rows as f64);
    let width = LONG_EDGE_PX;
    let tile_w = ((width - GUTTER_PX * (cols_f + 1.0)) / cols_f).floor();
    let tile_h = (tile_w * aspect).round();
    let height = tile_h * rows_f + GUTTER_PX * (rows_f + 1.0);
    let tiles = (0..n)
        .map(|i| {
            let (c, r) = ((i % cols) as f64, (i / cols) as f64);
            Tile {
                x: GUTTER_PX + c * (tile_w + GUTTER_PX),
                y: GUTTER_PX + r * (tile_h + GUTTER_PX),
                width: tile_w,
                height: tile_h,
            }
        })
        .collect();
    (width as usize, height.round() as usize, tiles)
}

/// Draws the frames and the sheet into `out_dir` (`frames/` is created with mode 0700).
pub fn render(frames: &[FrameInput<'_>], out_dir: &Path) -> Result<Rendered, SheetError> {
    if frames.is_empty() {
        return Err(SheetError::Empty);
    }
    if frames.len() > 6 {
        return Err(SheetError::TooMany(frames.len()));
    }
    let frames_dir = out_dir.join("frames");
    ensure_folder(out_dir)?;
    ensure_folder(&frames_dir)?;
    let fonts = Fonts::load();
    let mut draw_ms = 0u64;
    let mut encode_ms = 0u64;

    // Single frames at their own size, with their marks.
    let mut frame_paths = Vec::with_capacity(frames.len());
    for frame in frames {
        let t = Instant::now();
        let canvas = Canvas::new(frame.picture.clone())?;
        let tile = Tile {
            x: 0.0,
            y: 0.0,
            width: frame.picture.width as f64,
            height: frame.picture.height as f64,
        };
        draw_marks(&canvas, tile, frame.marks, &fonts, 1.0);
        draw_chip(
            canvas.ctx(),
            &format!("#{}", frame.n),
            &fonts.chip,
            CGPoint::new(tile.x + GUTTER_PX, tile.y + GUTTER_PX),
            5.0,
        );
        let bitmap = canvas.into_bitmap();
        draw_ms += t.elapsed().as_millis() as u64;
        let t = Instant::now();
        let path = frames_dir.join(format!("{}.png", frame.n));
        image::write_png(&bitmap, &path)?;
        encode_ms += t.elapsed().as_millis() as u64;
        frame_paths.push(path);
    }

    // The sheet: tiles in time order from the top left.
    let t = Instant::now();
    let first = frames.first().ok_or(SheetError::Empty)?.picture;
    let aspect = first.height as f64 / first.width.max(1) as f64;
    let (width, height, tiles) = sheet_layout(frames.len(), aspect);
    let canvas = Canvas::new(Bitmap::new(width, height)?)?;
    canvas.fill(1.0, 1.0, 1.0);
    for (frame, tile) in frames.iter().zip(&tiles) {
        canvas.draw_bitmap(
            frame.picture,
            CGRect::new(
                CGPoint::new(tile.x, tile.y),
                CGSize::new(tile.width, tile.height),
            ),
        )?;
        let scale = tile.width / frame.picture.width.max(1) as f64;
        draw_marks(&canvas, *tile, frame.marks, &fonts, scale);
        draw_chip(
            canvas.ctx(),
            &format!("#{}", frame.n),
            &fonts.chip,
            CGPoint::new(tile.x + 6.0, tile.y + 6.0),
            5.0,
        );
    }
    let sheet = canvas.into_bitmap();
    draw_ms += t.elapsed().as_millis() as u64;
    let t = Instant::now();
    let sheet_path = out_dir.join("contact-sheet.png");
    image::write_png(&sheet, &sheet_path)?;
    encode_ms += t.elapsed().as_millis() as u64;

    Ok(Rendered {
        sheet: sheet_path,
        frames: frame_paths,
        sheet_size: (width, height),
        draw_ms,
        encode_ms,
    })
}

fn ensure_folder(path: &Path) -> Result<(), SheetError> {
    if path.is_dir() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(FOLDER_MODE));
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(FOLDER_MODE)
        .create(path)
        .map_err(|e| SheetError::Folder(format!("{}: {e}", path.display())))
}

struct Fonts {
    chip: CFRetained<CTFont>,
    label: CFRetained<CTFont>,
}

impl Fonts {
    fn load() -> Fonts {
        Fonts {
            chip: font("HelveticaNeue-Bold", CHIP_PX),
            label: font("HiraginoSans-W6", CHIP_PX),
        }
    }
}

/// A font by PostScript name; CoreText falls back to a system font for an unknown name.
fn font(name: &str, size: f64) -> CFRetained<CTFont> {
    let name = CFString::from_str(name);
    // SAFETY: the name is a valid CFString and a null matrix means the identity.
    unsafe { CTFont::with_name(&name, size, std::ptr::null()) }
}

fn color(r: f64, g: f64, b: f64, a: f64) -> CFRetained<CGColor> {
    CGColor::new_srgb(r, g, b, a)
}

/// Pixel position of a normalised point inside a tile.
fn at(tile: Tile, p: Point) -> CGPoint {
    CGPoint::new(
        tile.x + tile.width * f64::from(p.0.clamp(0.0, 1.0)),
        tile.y + tile.height * f64::from(p.1.clamp(0.0, 1.0)),
    )
}

/// All marks of one keyframe, scaled for a tile (`scale` 1 on a frame).
fn draw_marks(canvas: &Canvas, tile: Tile, marks: &[Mark], fonts: &Fonts, scale: f64) {
    let ctx = canvas.ctx();
    for mark in marks {
        match mark {
            Mark::Ink { label, points, .. } => {
                let pts: Vec<CGPoint> = points.iter().map(|p| at(tile, *p)).collect();
                draw_ink(ctx, &pts, &ink_label(*label), &fonts.label, scale);
            }
            Mark::Click { at: p, .. } => {
                draw_ring(ctx, at(tile, *p), CLICK_RADIUS * scale, false, scale);
            }
            Mark::Dwell { at: p, .. } => {
                draw_ring(ctx, at(tile, *p), DWELL_RADIUS * scale, true, scale);
            }
        }
    }
}

/// A stroke: a white edge under a red core, round joins, and its circled number at the end.
fn draw_ink(ctx: &CGContext, points: &[CGPoint], label: &str, font: &CTFont, scale: f64) {
    let Some(first) = points.first() else {
        return;
    };
    let (er, eg, eb, ea) = INK_EDGE_RGBA;
    let (r, g, b) = INK_STROKE_RGB;
    for (width, (cr, cg, cb, ca)) in [
        ((INK_EDGE_WIDTH * scale).max(2.0), (er, eg, eb, ea)),
        ((INK_CORE_WIDTH * scale).max(1.5), (r, g, b, 1.0)),
    ] {
        CGContext::set_line_width(Some(ctx), width);
        CGContext::set_line_cap(Some(ctx), CGLineCap::Round);
        CGContext::set_line_join(Some(ctx), CGLineJoin::Round);
        CGContext::set_rgb_stroke_color(Some(ctx), cr, cg, cb, ca);
        CGContext::move_to_point(Some(ctx), first.x, first.y);
        if points.len() == 1 {
            // A tap: a dot.
            CGContext::add_line_to_point(Some(ctx), first.x + 0.5, first.y);
        }
        for p in points.iter().skip(1) {
            CGContext::add_line_to_point(Some(ctx), p.x, p.y);
        }
        CGContext::stroke_path(Some(ctx));
    }
    let end = points.last().unwrap_or(first);
    draw_chip(
        ctx,
        label,
        font,
        CGPoint::new(end.x + 6.0 * scale, end.y - 30.0 * scale),
        3.0,
    );
}

/// Click: a translucent ring with a centre dot. Dwell: a dotted ring.
fn draw_ring(ctx: &CGContext, at: CGPoint, radius: f64, dotted: bool, scale: f64) {
    let (r, g, b) = INK_STROKE_RGB;
    let rect = CGRect::new(
        CGPoint::new(at.x - radius, at.y - radius),
        CGSize::new(radius * 2.0, radius * 2.0),
    );
    CGContext::set_line_width(Some(ctx), (3.0 * scale).max(1.5));
    CGContext::set_rgb_stroke_color(Some(ctx), r, g, b, 0.75);
    if dotted {
        let lengths = [6.0 * scale as CGFloat, 5.0 * scale as CGFloat];
        // SAFETY: the lengths array outlives the call and its length is passed with it.
        unsafe { CGContext::set_line_dash(Some(ctx), 0.0, lengths.as_ptr(), lengths.len()) };
    }
    CGContext::stroke_ellipse_in_rect(Some(ctx), rect);
    // SAFETY: a null pattern with length 0 turns dashes off.
    unsafe { CGContext::set_line_dash(Some(ctx), 0.0, std::ptr::null(), 0) };
    if !dotted {
        let dot = (3.0 * scale).max(1.5);
        CGContext::set_rgb_fill_color(Some(ctx), r, g, b, 0.9);
        CGContext::fill_ellipse_in_rect(
            Some(ctx),
            CGRect::new(
                CGPoint::new(at.x - dot, at.y - dot),
                CGSize::new(dot * 2.0, dot * 2.0),
            ),
        );
    }
}

/// Text in a white rounded chip with a thin black border, `origin` at its top left.
fn draw_chip(ctx: &CGContext, text: &str, font: &CTFont, origin: CGPoint, pad: f64) -> CGSize {
    let black = color(0.0, 0.0, 0.0, 1.0);
    // SAFETY: the attribute keys are immortal CoreText constants; the values live for the call.
    let line = unsafe {
        let attrs: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[kCTFontAttributeName, kCTForegroundColorAttributeName],
            &[font.as_ref(), black.as_ref()],
        );
        let string = CFString::from_str(text);
        let Some(attributed) =
            CFAttributedString::new(None, Some(&string), Some(attrs.as_opaque()))
        else {
            return CGSize::new(0.0, 0.0);
        };
        CTLine::with_attributed_string(&attributed)
    };
    let (mut ascent, mut descent, mut leading) = (0.0, 0.0, 0.0);
    // SAFETY: the out-pointers are valid for the call.
    let width =
        unsafe { CTLine::typographic_bounds(&line, &mut ascent, &mut descent, &mut leading) };
    let size = CGSize::new(width + pad * 2.0, ascent + descent + pad * 1.2);
    let rect = CGRect::new(origin, size);
    // SAFETY: a null transform means the identity.
    let path = unsafe { CGPath::with_rounded_rect(rect, 5.0, 5.0, std::ptr::null()) };
    CGContext::set_rgb_fill_color(Some(ctx), 1.0, 1.0, 1.0, 0.95);
    CGContext::set_rgb_stroke_color(Some(ctx), 0.0, 0.0, 0.0, 1.0);
    CGContext::set_line_width(Some(ctx), 1.0);
    CGContext::add_path(Some(ctx), Some(&path));
    CGContext::fill_path(Some(ctx));
    CGContext::add_path(Some(ctx), Some(&path));
    CGContext::stroke_path(Some(ctx));
    // CoreText draws upward; flip locally inside the flipped canvas.
    CGContext::save_g_state(Some(ctx));
    CGContext::translate_ctm(Some(ctx), origin.x + pad, origin.y + pad * 0.6 + ascent);
    CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
    CGContext::set_text_position(Some(ctx), 0.0, 0.0);
    // SAFETY: the line and the context are valid.
    unsafe { CTLine::draw(&line, ctx) };
    CGContext::restore_g_state(Some(ctx));
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(width: usize, height: usize, level: u8) -> Bitmap {
        let mut b = Bitmap::new(width, height).unwrap();
        for px in b.bgra.as_chunks_mut::<4>().0 {
            px[0] = level;
            px[1] = level;
            px[2] = level;
        }
        b
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("murmur-sheet-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn is_reddish(px: (u8, u8, u8, u8)) -> bool {
        px.0 > 180 && px.1 < 120 && px.2 < 120
    }

    #[test]
    fn grid_by_count() {
        assert_eq!(grid_for(1), (1, 1));
        assert_eq!(grid_for(2), (2, 1));
        assert_eq!(grid_for(3), (2, 2));
        assert_eq!(grid_for(4), (2, 2));
        assert_eq!(grid_for(5), (3, 2));
        assert_eq!(grid_for(6), (3, 2));
    }

    #[test]
    fn layout_fills_the_width_with_gutters() {
        let (w, h, tiles) = sheet_layout(2, 0.6);
        assert_eq!(w, 1568);
        // (1568 - 3 * 8) / 2 = 772 wide, 463 high; one row plus two gutters.
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].width, 772.0);
        assert_eq!(tiles[0].height, 463.0);
        assert_eq!(tiles[1].x, 8.0 + 772.0 + 8.0);
        assert_eq!(h, 463 + 16);
        let (_, h, tiles) = sheet_layout(6, 0.5625);
        assert_eq!(tiles.len(), 6);
        assert_eq!(tiles[3].y, tiles[0].y + tiles[0].height + GUTTER_PX);
        assert_eq!(tiles[5].x, tiles[2].x);
        assert_eq!(h as f64, tiles[0].height * 2.0 + GUTTER_PX * 3.0);
        // A single picture takes the whole width.
        let (_, _, tiles) = sheet_layout(1, 0.5);
        assert_eq!(tiles[0].width, 1552.0);
    }

    #[test]
    fn ink_colour_matches_the_design_token() {
        let css =
            std::fs::read_to_string(format!("{}/../src/styles.css", env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        let token = css
            .lines()
            .find_map(|l| l.trim().strip_prefix("--ink-stroke:"))
            .map(|v| v.trim().trim_end_matches(';').to_owned())
            .expect("--ink-stroke token");
        let hex = token.trim_start_matches('#');
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f64 / 255.0;
        let (r, g, b) = INK_STROKE_RGB;
        assert!(
            (channel(0) - r).abs() < 1e-6
                && (channel(2) - g).abs() < 1e-6
                && (channel(4) - b).abs() < 1e-6,
            "{token}"
        );
        assert!(css.contains("--ink-stroke-edge:"));
    }

    #[test]
    fn render_writes_frames_and_a_sheet_with_visible_marks() {
        let a = gray(400, 240, 128);
        let b = gray(400, 240, 200);
        let marks_a = vec![
            Mark::Ink {
                t0_ms: 100,
                t1_ms: 400,
                keyframe: 1,
                label: 1,
                points: vec![(0.1, 0.1), (0.3, 0.3), (0.5, 0.5)],
            },
            Mark::Click {
                t_ms: 500,
                keyframe: 1,
                at: (0.8, 0.2),
            },
        ];
        let marks_b = vec![Mark::Dwell {
            t0_ms: 900,
            t1_ms: 1_600,
            keyframe: 2,
            at: (0.6, 0.7),
        }];
        let out = scratch().join("bundle");
        let rendered = render(
            &[
                FrameInput {
                    n: 1,
                    picture: &a,
                    marks: &marks_a,
                },
                FrameInput {
                    n: 2,
                    picture: &b,
                    marks: &marks_b,
                },
            ],
            &out,
        )
        .unwrap();
        assert_eq!(rendered.frames.len(), 2);
        assert_eq!(rendered.sheet_size, (1568, 479));
        let mode = std::fs::metadata(out.join("frames"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        // Frame 1: the stroke's middle is red, a far corner is the gray of the picture.
        let frame = image::read_png(&rendered.frames[0]).unwrap();
        assert_eq!((frame.width, frame.height), (400, 240));
        let mid = frame.pixel(120, 72).unwrap();
        assert!(is_reddish(mid), "stroke pixel {mid:?}");
        let corner = frame.pixel(390, 230).unwrap();
        assert_eq!((corner.0, corner.1, corner.2), (128, 128, 128));
        // The click's centre dot is red.
        let dot = frame.pixel(320, 48).unwrap();
        assert!(is_reddish(dot), "click dot {dot:?}");
        // The `#1` chip sits at the top left: white inside.
        let chip = frame.pixel(14, 14).unwrap();
        assert!(
            chip.0 > 200 && chip.1 > 200 && chip.2 > 200,
            "chip {chip:?}"
        );

        // The sheet: white gutters, the second tile holds the lighter picture, the stroke shows.
        let sheet = image::read_png(&rendered.sheet).unwrap();
        assert_eq!((sheet.width, sheet.height), (1568, 479));
        assert_eq!(sheet.pixel(2, 2), Some((255, 255, 255, 255)));
        let in_b = sheet.pixel(8 + 772 + 8 + 700, 400).unwrap();
        assert!(in_b.0 > 190 && in_b.0 < 210, "tile 2 pixel {in_b:?}");
        let stroke_on_sheet = sheet
            .pixel(8 + (772.0 * 0.3) as usize, 8 + (463.0 * 0.3) as usize)
            .unwrap();
        assert!(
            is_reddish(stroke_on_sheet),
            "sheet stroke {stroke_on_sheet:?}"
        );

        assert!(matches!(render(&[], &out), Err(SheetError::Empty)));
        std::fs::remove_dir_all(out.parent().unwrap()).unwrap();
    }
}
