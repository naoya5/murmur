//! Adapter: the pictures of an annotation bundle. Draws each picture with the ink groups that
//! belong to it (`frames/<n>.png`, each at its own size, long edge 1568 px as captured) with
//! CoreGraphics, CoreText and ImageIO, through the shared `image` helpers. No contact sheet:
//! the pictures are pasted one by one.
//!
//! Pictures and the words they go with never reach the log (G7). Nothing here panics (G6).

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use murmur_core::annotations::{MAX_KEYFRAMES, Mark, Point};
use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{CGColor, CGContext, CGLineCap, CGLineJoin, CGPath};
use objc2_core_text::{CTFont, CTLine, kCTFontAttributeName, kCTForegroundColorAttributeName};

use super::image::{self, Bitmap, Canvas, ImageError};

/// Height of the `#n` chip text (smaller was not readable to the models).
pub const CHIP_PX: f64 = 24.0;
/// Inset of the chip from the top left corner, and horizontal padding inside it.
const CHIP_INSET_PX: f64 = 8.0;
const CHIP_PAD_PX: f64 = 9.0;
/// The badge with a group's number: a filled circle in the stroke colour with a white digit.
/// 28 px: the models could not read Hex's 20 px chips, so the badge stays well above that.
pub const BADGE_PX: f64 = 28.0;
const BADGE_FONT_PX: f64 = 18.0;
/// Colour of an ink stroke: `--ink-stroke` in `src/styles.css` (a test keeps them equal).
pub const INK_STROKE_RGB: (f64, f64, f64) = (1.0, 59.0 / 255.0, 48.0 / 255.0);
/// Outer edge of a stroke, so it reads on dark screens too: `--ink-stroke-edge`.
pub const INK_EDGE_RGBA: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.92);
/// Stroke widths in pixels.
const INK_EDGE_WIDTH: f64 = 7.0;
const INK_CORE_WIDTH: f64 = 4.0;
/// Mode of the bundle folders.
const FOLDER_MODE: u32 = 0o700;

#[derive(Debug, thiserror::Error)]
pub enum DrawError {
    #[error("no picture to draw")]
    Empty,
    #[error("too many pictures: {0}")]
    TooMany(usize),
    #[error(transparent)]
    Image(#[from] ImageError),
    #[error("could not create {0}")]
    Folder(String),
}

/// One picture to draw: its pixels and the marks assigned to it.
pub struct FrameInput<'a> {
    /// The picture number, from 1.
    pub n: usize,
    pub picture: &'a Bitmap,
    pub marks: &'a [Mark],
}

/// What `render` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// `frames/1.png` .. in picture order.
    pub frames: Vec<PathBuf>,
    pub draw_ms: u64,
    pub encode_ms: u64,
}

/// Draws the pictures into `out_dir/frames/` (created with mode 0700).
pub fn render(frames: &[FrameInput<'_>], out_dir: &Path) -> Result<Rendered, DrawError> {
    if frames.is_empty() {
        return Err(DrawError::Empty);
    }
    if frames.len() > MAX_KEYFRAMES {
        return Err(DrawError::TooMany(frames.len()));
    }
    let frames_dir = out_dir.join("frames");
    ensure_folder(out_dir)?;
    ensure_folder(&frames_dir)?;
    let fonts = Fonts::load();
    let mut draw_ms = 0u64;
    let mut encode_ms = 0u64;
    let mut frame_paths = Vec::with_capacity(frames.len());
    for frame in frames {
        let t = Instant::now();
        let canvas = Canvas::new(frame.picture.clone())?;
        let size = (frame.picture.width as f64, frame.picture.height as f64);
        for mark in frame.marks {
            draw_mark(canvas.ctx(), size, mark, &fonts.badge);
        }
        draw_chip(
            canvas.ctx(),
            &format!("#{}", frame.n),
            &fonts.chip,
            CGPoint::new(CHIP_INSET_PX, CHIP_INSET_PX),
            CHIP_PAD_PX,
        );
        let bitmap = canvas.into_bitmap();
        draw_ms += t.elapsed().as_millis() as u64;
        let t = Instant::now();
        let path = frames_dir.join(format!("{}.png", frame.n));
        image::write_png(&bitmap, &path)?;
        encode_ms += t.elapsed().as_millis() as u64;
        frame_paths.push(path);
    }
    Ok(Rendered {
        frames: frame_paths,
        draw_ms,
        encode_ms,
    })
}

fn ensure_folder(path: &Path) -> Result<(), DrawError> {
    if path.is_dir() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(FOLDER_MODE));
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(FOLDER_MODE)
        .create(path)
        .map_err(|e| DrawError::Folder(format!("{}: {e}", path.display())))
}

struct Fonts {
    chip: CFRetained<CTFont>,
    badge: CFRetained<CTFont>,
}

impl Fonts {
    fn load() -> Fonts {
        Fonts {
            chip: font("HelveticaNeue-Bold", CHIP_PX),
            badge: font("HelveticaNeue-Bold", BADGE_FONT_PX),
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

/// Pixel position of a normalised point on a picture of `size`.
fn at(size: (f64, f64), p: Point) -> CGPoint {
    CGPoint::new(
        size.0 * f64::from(p.0.clamp(0.0, 1.0)),
        size.1 * f64::from(p.1.clamp(0.0, 1.0)),
    )
}

/// One ink group: every stroke as a white edge under a red core, round joins, and the
/// group's number badge at the end of its last stroke.
fn draw_mark(ctx: &CGContext, size: (f64, f64), mark: &Mark, font: &CTFont) {
    let mut end: Option<CGPoint> = None;
    for stroke in &mark.strokes {
        let points: Vec<CGPoint> = stroke.iter().map(|p| at(size, *p)).collect();
        let Some(first) = points.first() else {
            continue;
        };
        let (er, eg, eb, ea) = INK_EDGE_RGBA;
        let (r, g, b) = INK_STROKE_RGB;
        for (width, (cr, cg, cb, ca)) in [
            (INK_EDGE_WIDTH, (er, eg, eb, ea)),
            (INK_CORE_WIDTH, (r, g, b, 1.0)),
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
        end = points.last().copied().or(end);
    }
    if let Some(end) = end {
        draw_badge(
            ctx,
            &mark.label.to_string(),
            font,
            CGPoint::new(end.x + 18.0, end.y - 18.0),
        );
    }
}

/// A filled circle in the stroke colour, a thin white edge, and a white bold number inside.
fn draw_badge(ctx: &CGContext, text: &str, font: &CTFont, center: CGPoint) {
    let (r, g, b) = INK_STROKE_RGB;
    let radius = BADGE_PX / 2.0;
    let rect = CGRect::new(
        CGPoint::new(center.x - radius, center.y - radius),
        CGSize::new(BADGE_PX, BADGE_PX),
    );
    CGContext::set_rgb_fill_color(Some(ctx), r, g, b, 1.0);
    CGContext::fill_ellipse_in_rect(Some(ctx), rect);
    CGContext::set_line_width(Some(ctx), 1.5);
    CGContext::set_rgb_stroke_color(Some(ctx), 1.0, 1.0, 1.0, 0.95);
    CGContext::stroke_ellipse_in_rect(Some(ctx), rect);
    let white = color(1.0, 1.0, 1.0, 1.0);
    // SAFETY: the attribute keys are immortal CoreText constants; the values live for the call.
    let line = unsafe {
        let attrs: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
            &[kCTFontAttributeName, kCTForegroundColorAttributeName],
            &[font.as_ref(), white.as_ref()],
        );
        let string = CFString::from_str(text);
        let Some(attributed) =
            CFAttributedString::new(None, Some(&string), Some(attrs.as_opaque()))
        else {
            return;
        };
        CTLine::with_attributed_string(&attributed)
    };
    let (mut ascent, mut descent, mut leading) = (0.0, 0.0, 0.0);
    // SAFETY: the out-pointers are valid for the call.
    let width =
        unsafe { CTLine::typographic_bounds(&line, &mut ascent, &mut descent, &mut leading) };
    CGContext::save_g_state(Some(ctx));
    CGContext::translate_ctm(
        Some(ctx),
        center.x - width / 2.0,
        center.y + (ascent - descent) / 2.0,
    );
    CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
    CGContext::set_text_position(Some(ctx), 0.0, 0.0);
    // SAFETY: the line and the context are valid.
    unsafe { CTLine::draw(&line, ctx) };
    CGContext::restore_g_state(Some(ctx));
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
        let dir = std::env::temp_dir().join(format!("murmur-draw-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn is_reddish(px: (u8, u8, u8, u8)) -> bool {
        px.0 > 180 && px.1 < 120 && px.2 < 120
    }

    #[test]
    fn ink_colour_matches_the_design_token() {
        let css = include_str!("../../../src/styles.css");
        let token = css
            .lines()
            .find_map(|l| l.trim().strip_prefix("--ink-stroke:"))
            .map(|v| v.trim().trim_end_matches(';'))
            .expect("--ink-stroke in styles.css");
        let (r, g, b) = INK_STROKE_RGB;
        let hex = format!(
            "#{:02x}{:02x}{:02x}",
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8
        );
        assert_eq!(token.to_lowercase(), hex);
    }

    #[test]
    fn render_writes_each_picture_with_visible_marks_and_no_sheet() {
        let a = gray(400, 240, 128);
        let b = gray(400, 240, 200);
        let marks_a = vec![
            Mark {
                keyframe: 1,
                label: 1,
                t0_ms: 100,
                t1_ms: 400,
                strokes: vec![vec![(0.1, 0.1), (0.3, 0.3), (0.5, 0.5)]],
            },
            Mark {
                keyframe: 1,
                label: 2,
                t0_ms: 900,
                t1_ms: 1_000,
                strokes: vec![vec![(0.8, 0.8)]],
            },
        ];
        let marks_b = vec![Mark {
            keyframe: 2,
            label: 3,
            t0_ms: 2_000,
            t1_ms: 2_400,
            strokes: vec![vec![(0.6, 0.7), (0.7, 0.7)], vec![(0.6, 0.8), (0.7, 0.8)]],
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
        assert!(!out.join("contact-sheet.png").exists());
        let mode = std::fs::metadata(out.join("frames"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        // Picture 1 keeps its size; the stroke's middle is red; a far corner is the gray.
        let frame = image::read_png(&rendered.frames[0]).unwrap();
        assert_eq!((frame.width, frame.height), (400, 240));
        let mid = frame.pixel(120, 72).unwrap();
        assert!(is_reddish(mid), "stroke pixel {mid:?}");
        let corner = frame.pixel(390, 230).unwrap();
        assert_eq!((corner.0, corner.1, corner.2), (128, 128, 128));
        // A tap is a dot.
        let tap = frame.pixel(320, 192).unwrap();
        assert!(is_reddish(tap), "tap pixel {tap:?}");
        // The `#1` chip sits at the top left: white inside.
        let chip = frame.pixel(14, 14).unwrap();
        assert!(
            chip.0 > 200 && chip.1 > 200 && chip.2 > 200,
            "chip {chip:?}"
        );
        // Picture 2: both strokes of the group are drawn.
        let frame = image::read_png(&rendered.frames[1]).unwrap();
        assert!(is_reddish(frame.pixel(260, 168).unwrap()));
        assert!(is_reddish(frame.pixel(260, 192).unwrap()));

        assert!(matches!(render(&[], &out), Err(DrawError::Empty)));
        std::fs::remove_dir_all(out.parent().unwrap()).unwrap();
    }

    #[test]
    fn more_pictures_than_the_cap_are_refused() {
        let p = gray(40, 24, 100);
        let frames: Vec<FrameInput<'_>> = (1..=MAX_KEYFRAMES + 1)
            .map(|n| FrameInput {
                n,
                picture: &p,
                marks: &[],
            })
            .collect();
        let out = scratch();
        assert!(matches!(
            render(&frames, &out),
            Err(DrawError::TooMany(n)) if n == MAX_KEYFRAMES + 1
        ));
        std::fs::remove_dir_all(out).unwrap();
    }
}
