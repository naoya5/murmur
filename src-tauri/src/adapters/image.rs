//! Shared bitmap helpers for the annotation adapters: a BGRA bitmap in memory, PNG in and out
//! through ImageIO, and a CoreGraphics canvas that draws into such a bitmap.
//!
//! Not an adapter of its own: `screen.rs` writes candidate pictures through it and `sheet.rs`
//! draws the keyframes and the contact sheet with it. Pictures never reach the log (G7).

use std::ffi::c_void;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use objc2_core_foundation::{CFRetained, CFString, CFURL, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGBitmapInfo, CGColorSpace, CGContext,
    CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality,
};
use objc2_image_io::{CGImageDestination, CGImageSource};

/// Mode of every picture file: the user alone can read it.
pub const FILE_MODE: u32 = 0o600;

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("bitmap of {0}x{1} is empty or too large")]
    Size(usize, usize),
    #[error("could not create a drawing context")]
    Context,
    #[error("could not read {0}")]
    Read(String),
    #[error("could not write {0}")]
    Write(String),
}

/// Pixels in memory: premultiplied BGRA, 8 bits each, rows of `width * 4` bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitmap({}x{})", self.width, self.height)
    }
}

impl Bitmap {
    /// A black, opaque bitmap.
    pub fn new(width: usize, height: usize) -> Result<Bitmap, ImageError> {
        let bytes = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n > 0 && *n <= 512 * 1024 * 1024)
            .ok_or(ImageError::Size(width, height))?;
        let mut bgra = vec![0u8; bytes];
        for px in bgra.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        Ok(Bitmap {
            width,
            height,
            bgra,
        })
    }

    /// Copies rows of a BGRA buffer whose rows may be padded (`bytes_per_row >= width * 4`).
    pub fn from_rows(
        width: usize,
        height: usize,
        bytes_per_row: usize,
        data: &[u8],
    ) -> Result<Bitmap, ImageError> {
        let mut bitmap = Bitmap::new(width, height)?;
        let row = width * 4;
        if bytes_per_row < row || data.len() < bytes_per_row * height {
            return Err(ImageError::Size(width, height));
        }
        for (y, dst) in bitmap.bgra.chunks_exact_mut(row).enumerate() {
            let start = y * bytes_per_row;
            dst.copy_from_slice(&data[start..start + row]);
        }
        Ok(bitmap)
    }

    /// The pixel at (x, y) as (r, g, b, a); `None` outside the bitmap.
    pub fn pixel(&self, x: usize, y: usize) -> Option<(u8, u8, u8, u8)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y * self.width + x) * 4;
        let px = self.bgra.get(i..i + 4)?;
        Some((px[2], px[1], px[0], px[3]))
    }
}

/// A CoreGraphics context that draws into a `Bitmap`. The coordinate space is flipped so that
/// y grows downward, as in the bitmap and in the normalised mark positions.
pub struct Canvas {
    bitmap: Bitmap,
    ctx: CFRetained<CGContext>,
}

impl Canvas {
    pub fn new(bitmap: Bitmap) -> Result<Canvas, ImageError> {
        let mut bitmap = bitmap;
        let space = CGColorSpace::new_device_rgb().ok_or(ImageError::Context)?;
        let info = CGBitmapInfo(CGImageAlphaInfo::PremultipliedFirst.0)
            | CGBitmapInfo(CGImageByteOrderInfo::Order32Little.0);
        // SAFETY: the buffer is `width * 4 * height` bytes, owned by `bitmap`, which lives as
        // long as the context (both are fields of the returned `Canvas`) and is never resized.
        let ctx = unsafe {
            CGBitmapContextCreate(
                bitmap.bgra.as_mut_ptr() as *mut c_void,
                bitmap.width,
                bitmap.height,
                8,
                bitmap.width * 4,
                Some(&space),
                info.0,
            )
        }
        .ok_or(ImageError::Context)?;
        CGContext::translate_ctm(Some(&ctx), 0.0, bitmap.height as f64);
        CGContext::scale_ctm(Some(&ctx), 1.0, -1.0);
        Ok(Canvas { bitmap, ctx })
    }

    pub fn ctx(&self) -> &CGContext {
        &self.ctx
    }

    /// Fills the whole canvas with an opaque colour (components 0 to 1).
    pub fn fill(&self, r: f64, g: f64, b: f64) {
        CGContext::set_rgb_fill_color(Some(&self.ctx), r, g, b, 1.0);
        CGContext::fill_rect(
            Some(&self.ctx),
            CGRect::new(
                CGPoint::new(0.0, 0.0),
                CGSize::new(self.bitmap.width as f64, self.bitmap.height as f64),
            ),
        );
    }

    /// Draws `source` scaled into `rect` (top-left origin). A 1:1 blit is not resampled.
    pub fn draw_bitmap(&self, source: &Bitmap, rect: CGRect) -> Result<(), ImageError> {
        let image = cg_image(source)?;
        let ctx = &self.ctx;
        CGContext::save_g_state(Some(ctx));
        // The canvas is flipped; images are stored top-down, so flip back inside the tile.
        CGContext::translate_ctm(Some(ctx), rect.origin.x, rect.origin.y + rect.size.height);
        CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
        let scaled = (source.width as f64 - rect.size.width).abs() > 0.5;
        CGContext::set_interpolation_quality(
            Some(ctx),
            if scaled {
                CGInterpolationQuality::High
            } else {
                CGInterpolationQuality::None
            },
        );
        CGContext::draw_image(
            Some(ctx),
            CGRect::new(CGPoint::new(0.0, 0.0), rect.size),
            Some(&image),
        );
        CGContext::restore_g_state(Some(ctx));
        Ok(())
    }

    /// The drawn pixels. The context is released first, so the bytes are final.
    pub fn into_bitmap(self) -> Bitmap {
        let Canvas { bitmap, ctx } = self;
        drop(ctx);
        bitmap
    }

    /// A snapshot of the drawing as a CoreGraphics image (a copy).
    fn image(&self) -> Result<CFRetained<CGImage>, ImageError> {
        CGBitmapContextCreateImage(Some(&self.ctx)).ok_or(ImageError::Context)
    }
}

/// A CoreGraphics image of the bitmap (copies the pixels).
pub fn cg_image(bitmap: &Bitmap) -> Result<CFRetained<CGImage>, ImageError> {
    let canvas = Canvas::new(bitmap.clone())?;
    canvas.image()
}

/// Writes the bitmap as PNG with mode 0600. The parent folder must exist.
pub fn write_png(bitmap: &Bitmap, path: &Path) -> Result<(), ImageError> {
    let image = cg_image(bitmap)?;
    write_cg_image(&image, path)
}

fn write_cg_image(image: &CGImage, path: &Path) -> Result<(), ImageError> {
    let shown = path.display().to_string();
    let url = CFURL::from_file_path(path).ok_or_else(|| ImageError::Write(shown.clone()))?;
    let kind = CFString::from_static_str("public.png");
    // SAFETY: the URL and the type are valid for the duration of the call; the destination is
    // used from this thread only and dropped after `finalize`.
    let ok = unsafe {
        let Some(dest) = CGImageDestination::with_url(&url, &kind, 1, None) else {
            return Err(ImageError::Write(shown));
        };
        CGImageDestination::add_image(&dest, image, None);
        CGImageDestination::finalize(&dest)
    };
    if !ok {
        return Err(ImageError::Write(shown));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))
        .map_err(|e| ImageError::Write(format!("{shown}: {e}")))
}

/// Reads a PNG (or any image ImageIO decodes) into a bitmap.
pub fn read_png(path: &Path) -> Result<Bitmap, ImageError> {
    let shown = path.display().to_string();
    let url = CFURL::from_file_path(path).ok_or_else(|| ImageError::Read(shown.clone()))?;
    // SAFETY: the URL is valid; the source and the decoded image are plain CF objects owned here.
    let image = unsafe {
        let source =
            CGImageSource::with_url(&url, None).ok_or_else(|| ImageError::Read(shown.clone()))?;
        CGImageSource::image_at_index(&source, 0, None)
            .ok_or_else(|| ImageError::Read(shown.clone()))?
    };
    let width = CGImage::width(Some(&image));
    let height = CGImage::height(Some(&image));
    let canvas = Canvas::new(Bitmap::new(width, height)?)?;
    let ctx = canvas.ctx();
    CGContext::save_g_state(Some(ctx));
    CGContext::translate_ctm(Some(ctx), 0.0, height as f64);
    CGContext::scale_ctm(Some(ctx), 1.0, -1.0);
    CGContext::set_interpolation_quality(Some(ctx), CGInterpolationQuality::None);
    CGContext::draw_image(
        Some(ctx),
        CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(width as f64, height as f64),
        ),
        Some(&image),
    );
    CGContext::restore_g_state(Some(ctx));
    Ok(canvas.into_bitmap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("murmur-image-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bitmap_sizes_are_checked() {
        assert!(Bitmap::new(0, 10).is_err());
        assert!(Bitmap::new(100_000, 100_000).is_err());
        let b = Bitmap::new(3, 2).unwrap();
        assert_eq!(b.bgra.len(), 24);
        assert_eq!(b.pixel(0, 0), Some((0, 0, 0, 255)));
        assert_eq!(b.pixel(3, 0), None);
    }

    #[test]
    fn padded_rows_are_copied_tightly() {
        // 2 x 2 pixels with 12 bytes per row (4 bytes of padding).
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(&[10, 20, 30, 255]); // (0,0) BGRA
        data[12..16].copy_from_slice(&[40, 50, 60, 255]); // (0,1)
        let b = Bitmap::from_rows(2, 2, 12, &data).unwrap();
        assert_eq!(b.pixel(0, 0), Some((30, 20, 10, 255)));
        assert_eq!(b.pixel(0, 1), Some((60, 50, 40, 255)));
        assert!(Bitmap::from_rows(2, 2, 8, &data[..10]).is_err());
    }

    #[test]
    fn canvas_draws_with_the_top_left_origin_and_png_round_trips() {
        let canvas = Canvas::new(Bitmap::new(20, 10).unwrap()).unwrap();
        canvas.fill(1.0, 1.0, 1.0);
        // A red square in the top-left corner (y grows downward).
        CGContext::set_rgb_fill_color(Some(canvas.ctx()), 1.0, 0.0, 0.0, 1.0);
        CGContext::fill_rect(
            Some(canvas.ctx()),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(5.0, 5.0)),
        );
        let bitmap = canvas.into_bitmap();
        assert_eq!(bitmap.pixel(2, 2), Some((255, 0, 0, 255)));
        assert_eq!(bitmap.pixel(2, 8), Some((255, 255, 255, 255)));
        assert_eq!(bitmap.pixel(15, 2), Some((255, 255, 255, 255)));

        let dir = scratch();
        let path = dir.join("a.png");
        write_png(&bitmap, &path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = read_png(&path).unwrap();
        assert_eq!(back.width, 20);
        assert_eq!(back.height, 10);
        assert_eq!(back.pixel(2, 2), Some((255, 0, 0, 255)));
        assert_eq!(back.pixel(2, 8), Some((255, 255, 255, 255)));
        assert!(read_png(&dir.join("missing.png")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn draw_bitmap_scales_into_the_tile() {
        // A 4 x 2 source: left half red, right half blue.
        let mut src = Bitmap::new(4, 2).unwrap();
        for y in 0..2 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                let (b, g, r) = if x < 2 { (0, 0, 255) } else { (255, 0, 0) };
                src.bgra[i..i + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        let canvas = Canvas::new(Bitmap::new(40, 40).unwrap()).unwrap();
        canvas.fill(1.0, 1.0, 1.0);
        canvas
            .draw_bitmap(
                &src,
                CGRect::new(CGPoint::new(10.0, 10.0), CGSize::new(20.0, 10.0)),
            )
            .unwrap();
        let out = canvas.into_bitmap();
        let (r, _, b, _) = out.pixel(12, 15).unwrap();
        assert!(
            r > 200 && b < 60,
            "left half is red: {:?}",
            out.pixel(12, 15)
        );
        let (r, _, b, _) = out.pixel(27, 15).unwrap();
        assert!(
            b > 200 && r < 60,
            "right half is blue: {:?}",
            out.pixel(27, 15)
        );
        assert_eq!(out.pixel(5, 5), Some((255, 255, 255, 255)));
        assert_eq!(out.pixel(12, 25), Some((255, 255, 255, 255)));
    }
}
