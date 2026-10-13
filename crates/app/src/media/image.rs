//! Decoding still images to RGBA pixels. Drawing them is `PaintCx::image`'s
//! job; this is the decode side. [`ImageDecoder`] is the seam; the `image`
//! crate sits behind it in [`image_crate`] and nothing else names that crate.

mod image_crate;

use std::path::Path;

pub use image_crate::ImageCrate;

/// A decoded image: straight (not premultiplied) RGBA8 rows, top to bottom,
/// upright per the file's orientation tag.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The size the file declares, after orientation.
    pub source_width: u32,
    pub source_height: u32,
    /// The size of `rgba`: the source size, or smaller if it was fitted.
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Decoded {
    /// A decoded image, checked: no side is zero and `rgba` holds exactly
    /// `width * height` pixels. (A decoder is handed untrusted files, and
    /// textures and mips built from an empty or short buffer panic.)
    pub fn new(source: (u32, u32), (width, height): (u32, u32), rgba: Vec<u8>) -> Result<Self, String> {
        check_size(source.0, source.1)?;
        check_size(width, height)?;
        if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
            return Err("image data doesn't match its size".into());
        }
        Ok(Decoded { source_width: source.0, source_height: source.1, width, height, rgba })
    }
}

/// An image with a side of zero has no pixels: an error, not a panic later.
fn check_size(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        Err("empty image".into())
    } else {
        Ok(())
    }
}

/// Something that decodes image files.
pub trait ImageDecoder {
    /// Decode the image at `path`, upright per its EXIF orientation. One larger
    /// than `max_side` on either side is shrunk to fit, keeping its aspect
    /// ratio.
    fn decode(&self, path: &Path, max_side: u32) -> Result<Decoded, String>;
}

/// Decode with the default decoder: [`ImageDecoder::decode`] on [`ImageCrate`].
pub fn decode(path: &Path, max_side: u32) -> Result<Decoded, String> {
    ImageCrate.decode(path, max_side)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testing;
    use crate::plugin::testing::tmpdir;

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = tmpdir("media-image").join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_png_decodes_to_rgba_pixels() {
        let path = write("a.png", &testing::png(3, 2, [10, 20, 30, 255]));
        let d = decode(&path, 100).unwrap();
        assert_eq!((d.width, d.height, d.source_width, d.source_height), (3, 2, 3, 2));
        assert_eq!(d.rgba.len(), 3 * 2 * 4);
        assert!(d.rgba.chunks(4).all(|px| px == [10, 20, 30, 255]));
    }

    #[test]
    fn a_gif_decodes_to_rgba_pixels() {
        let path = write("a.gif", &testing::gif(4, 5, [200, 100, 0, 255]));
        let d = decode(&path, 100).unwrap();
        assert_eq!((d.width, d.height), (4, 5));
        assert!(d.rgba.chunks(4).all(|px| px == [200, 100, 0, 255]), "{:?}", &d.rgba[..4]);
    }

    #[test]
    fn an_image_over_the_cap_shrinks_and_reports_its_source_size() {
        let path = write("wide.png", &testing::png(40, 10, [255, 0, 0, 255]));
        let d = decode(&path, 20).unwrap();
        assert_eq!((d.source_width, d.source_height), (40, 10));
        assert_eq!((d.width, d.height), (20, 5), "fitted, aspect ratio kept");
        assert_eq!(d.rgba.len(), 20 * 5 * 4);
        let exact = decode(&path, 40).unwrap();
        assert_eq!((exact.width, exact.height), (40, 10), "at the cap is untouched");
    }

    #[test]
    fn a_file_that_is_not_an_image_is_an_error() {
        let path = write("bad.png", b"not a png");
        assert!(decode(&path, 100).is_err());
        assert!(decode(&path.with_file_name("missing.png"), 100).is_err());
    }

    #[test]
    fn an_image_with_no_pixels_is_an_error_not_a_panic() {
        // A fuzzed 17x0 GIF that used to panic building mips.
        let path = write("zero-height.gif", include_bytes!("testdata/zero-height.gif"));
        assert_eq!(decode(&path, 100), Err("empty image".to_string()));
    }

    #[test]
    fn decoded_images_are_checked() {
        assert!(Decoded::new((1, 1), (1, 1), vec![0; 4]).is_ok());
        assert_eq!(Decoded::new((0, 4), (0, 4), vec![]), Err("empty image".to_string()));
        assert_eq!(Decoded::new((4, 4), (4, 0), vec![]), Err("empty image".to_string()));
        assert!(Decoded::new((2, 2), (2, 2), vec![0; 15]).is_err(), "short data");
    }

    /// A decoder of our own plugs in wherever an [`ImageDecoder`] is taken.
    #[test]
    fn a_decoder_can_be_replaced() {
        struct Flat;
        impl ImageDecoder for Flat {
            fn decode(&self, _: &Path, _: u32) -> Result<Decoded, String> {
                Decoded::new((2, 1), (2, 1), vec![7; 8])
            }
        }
        let decoders: [&dyn ImageDecoder; 2] = [&Flat, &ImageCrate];
        assert_eq!(decoders[0].decode(Path::new("anything"), 1).unwrap().rgba, vec![7; 8]);
        assert!(decoders[1].decode(Path::new("/nonexistent/file.png"), 1).is_err());
    }
}
