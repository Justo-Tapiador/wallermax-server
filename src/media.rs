//! The media library's image pipeline (F9, the `[cms] media_*` keys).
//!
//! Pure, side-effect-free image work shared by the upload route: the
//! accepted-format whitelist, the magic-byte sniff, the
//! dimension-and-allocation-bounded decode, and the thumbnail
//! generation. Disk and database stay in `src/routes/media.rs`; this
//! module decides what is safe to accept, and nothing else.
//!
//! **Whitelist, then decode.** The sniffed format must be one of PNG,
//! JPEG, GIF or WebP (`MediaFormat::from_image_format`) — the mime type
//! stored (and later served) is derived from the *sniffed* format, never
//! from the client's headers, so a renamed or mislabeled upload can
//! never be served back as a lie. SVG is deliberately absent: it is
//! text, it can carry scripts, and the CMS's CSP-blocks-scripts story
//! should not depend on sanitizing an attacker-controlled one.
//!
//! **Decode is the second gate.** A file that sniffs as PNG but is
//! truncated or corrupt never reaches the disk: `prepare` fully decodes
//! it under [`image::Limits`] (bounded width, height and allocation —
//! the decompression-bomb guard), and only a decoded image becomes a
//! library row. The same call produces the PNG thumbnail the admin
//! listing serves, so every stored file has been proven decodable
//! before it is ever written.

use image::DynamicImage;

/// Highest accepted image dimension, per side. Generous for corporate
/// photography (8K-class), bounded enough to cap the decode cost.
const MAX_DIMENSION: u32 = 8_192;

/// Ceiling on the total decoder allocation (the decompression-bomb
/// backstop: 8192 × 8192 RGBA is ~268 MiB, still under the cap).
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// Longest edge of the admin-listing thumbnail (aspect ratio kept).
const THUMBNAIL_EDGE: u32 = 320;

/// The formats the media library accepts, with their stored mime type
/// and canonical extension (the on-disk name is server-generated, so
/// the extension is a projection of the sniff, not of the upload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl MediaFormat {
    /// The whitelist itself: maps a sniffed [`image::ImageFormat`] to
    /// the accepted set (`None` = rejected format).
    pub(crate) fn from_image_format(format: image::ImageFormat) -> Option<Self> {
        match format {
            image::ImageFormat::Png => Some(Self::Png),
            image::ImageFormat::Jpeg => Some(Self::Jpeg),
            image::ImageFormat::Gif => Some(Self::Gif),
            image::ImageFormat::WebP => Some(Self::Webp),
            _ => None,
        }
    }

    /// The mime type stored in the database and served back.
    pub(crate) fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// The canonical extension of the stored file name.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }
}

/// A fully validated upload, ready to be written to disk: the sniffed
/// format, the source dimensions (served as width/height attributes)
/// and the PNG thumbnail bytes.
#[derive(Debug)]
pub(crate) struct PreparedImage {
    pub(crate) format: MediaFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) thumbnail: Vec<u8>,
}

/// The decode limits: bounded dimensions and bounded allocation.
fn decode_limits() -> image::Limits {
    // `Limits` is `#[non_exhaustive]`: built from the default and
    // tightened field by field (the default already caps allocations
    // at 512 MiB, but being explicit keeps the story in one place).
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

/// Sniffs, whitelists and decodes an upload, producing the metadata and
/// the thumbnail. Every failure is the Spanish sentence the upload form
/// renders inline (the browser-facing convention of the whole panel).
pub(crate) fn prepare(bytes: &[u8]) -> Result<PreparedImage, &'static str> {
    let sniffed = image::guess_format(bytes).map_err(|_| {
        "El archivo no parece una imagen válida (la firma de bytes no se reconoce)."
    })?;
    let format = MediaFormat::from_image_format(sniffed)
        .ok_or("Formato no admitido: la biblioteca acepta PNG, JPEG, GIF y WebP.")?;

    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), sniffed);
    reader.limits(decode_limits());
    let decoded = reader.decode().map_err(|error| match error {
        image::ImageError::Limits(_) => {
            "La imagen es demasiado grande para procesarla con seguridad (máximo \
                 8192 × 8192 píxeles)."
        }
        _ => "La imagen está dañada o incompleta: no se pudo decodificar.",
    })?;

    let width = decoded.width();
    let height = decoded.height();
    let thumbnail = thumbnail_png(&decoded)?;

    Ok(PreparedImage {
        format,
        width,
        height,
        thumbnail,
    })
}

/// Renders the admin-listing thumbnail as PNG (aspect ratio preserved,
/// longest edge capped at [`THUMBNAIL_EDGE`]). Animated GIFs keep their
/// animation in the stored file; the thumbnail is the first frame.
///
/// Sources already smaller than the thumbnail box are re-encoded as-is:
/// the image crate's `thumbnail` follows box semantics and would
/// **upscale** them (a 1 × 1 icon would become 320 × 320), and the
/// thumbnail exists to bound the listing's cost, not to enlarge.
fn thumbnail_png(image: &DynamicImage) -> Result<Vec<u8>, &'static str> {
    let thumb = if image.width() <= THUMBNAIL_EDGE && image.height() <= THUMBNAIL_EDGE {
        image.clone()
    } else {
        image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    };
    let mut buffer = Vec::new();
    thumb
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .map_err(|_| "No se pudo generar la miniatura.")?;
    Ok(buffer)
}

/// A fresh server-generated name stem (32 lowercase hex characters from
/// a UUID v4). The stored file names are `<stem>.<ext>` and
/// `<stem>_t.png` — flat, unique and never user-influenced, so the
/// serving route can never be talked into joining a client-chosen path
/// segment onto the media directory.
pub(crate) fn new_stem() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a `width × height` RGB gradient as PNG in memory.
    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let mut image = image::RgbImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let value = ((x + y) % 256) as u8;
            *pixel = image::Rgb([value, 255 - value, value / 2]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("fixture encodes");
        bytes
    }

    #[test]
    fn the_whitelist_maps_the_four_formats() {
        use image::ImageFormat;
        assert_eq!(
            MediaFormat::from_image_format(ImageFormat::Png),
            Some(MediaFormat::Png)
        );
        assert_eq!(
            MediaFormat::from_image_format(ImageFormat::Jpeg),
            Some(MediaFormat::Jpeg)
        );
        assert_eq!(
            MediaFormat::from_image_format(ImageFormat::Gif),
            Some(MediaFormat::Gif)
        );
        assert_eq!(
            MediaFormat::from_image_format(ImageFormat::WebP),
            Some(MediaFormat::Webp)
        );
        for rejected in [ImageFormat::Bmp, ImageFormat::Tiff, ImageFormat::Pnm] {
            assert_eq!(MediaFormat::from_image_format(rejected), None);
        }
        assert_eq!(MediaFormat::Jpeg.mime(), "image/jpeg");
        assert_eq!(MediaFormat::Jpeg.extension(), "jpg");
        assert_eq!(MediaFormat::Webp.mime(), "image/webp");
        assert_eq!(MediaFormat::Png.extension(), "png");
    }

    #[test]
    fn prepare_decodes_png_with_dimensions_and_thumbnail() {
        let prepared = prepare(&png_fixture(400, 300)).expect("a well-formed PNG is accepted");
        assert_eq!(prepared.format, MediaFormat::Png);
        assert_eq!(prepared.width, 400);
        assert_eq!(prepared.height, 300);

        // The thumbnail parses back as a PNG with the capped size and
        // the 4:3 aspect ratio kept.
        let thumb = image::load_from_memory(&prepared.thumbnail).expect("thumbnail decodes");
        assert_eq!(thumb.width(), 320);
        assert_eq!(thumb.height(), 240);
    }

    #[test]
    fn prepare_keeps_tiny_images_as_is() {
        let prepared = prepare(&png_fixture(1, 1)).expect("1×1 is a valid image");
        let thumb = image::load_from_memory(&prepared.thumbnail).expect("thumbnail decodes");
        assert_eq!((thumb.width(), thumb.height()), (1, 1));
    }

    #[test]
    fn prepare_rejects_non_images() {
        let error = prepare(b"esto no es una imagen, solo texto").expect_err("text rejected");
        assert!(error.contains("no parece una imagen"), "{error}");
    }

    #[test]
    fn prepare_rejects_disallowed_formats() {
        // A well-formed BMP magic header (BM + a bogus DIB size).
        let bmp = b"BM\x36\x00\x00\x00\x00\x00\x00\x00\x28\x00\x00\x00".to_vec();
        let error = prepare(&bmp).expect_err("BMP is outside the whitelist");
        assert!(error.contains("Formato no admitido"), "{error}");
    }

    #[test]
    fn prepare_rejects_truncated_pngs() {
        let mut bytes = png_fixture(50, 50);
        bytes.truncate(bytes.len() / 2);
        let error = prepare(&bytes).expect_err("a truncated image never reaches the disk");
        assert!(error.contains("no se pudo decodificar"), "{error}");
    }

    #[test]
    fn prepare_rejects_dimension_bombs() {
        // A CRC-valid PNG header claiming 20000 × 20000: the header
        // parses cleanly, so the decoder reaches its limits check and
        // refuses before allocating anything.
        let bomb = png_header_claiming(20_000, 20_000);
        let error = prepare(&bomb).expect_err("oversized dimensions are refused");
        assert!(error.contains("demasiado grande"), "{error}");
    }

    /// The IEEE CRC-32 the PNG chunk format uses (bitwise, table-less —
    /// fine for a 17-byte header in a test).
    fn png_crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// PNG signature + a CRC-valid IHDR claiming `width × height`
    /// (8-bit RGB, no interlace) and nothing else.
    fn png_header_claiming(width: u32, height: u32) -> Vec<u8> {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(b"IHDR");
        chunk.extend_from_slice(&width.to_be_bytes());
        chunk.extend_from_slice(&height.to_be_bytes());
        chunk.extend_from_slice(&[8, 2, 0, 0, 0]);
        let crc = png_crc32(&chunk);

        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&chunk);
        png.extend_from_slice(&crc.to_be_bytes());
        png
    }

    #[test]
    fn stems_are_flat_hex_and_unique() {
        let first = new_stem();
        let second = new_stem();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
