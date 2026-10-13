//! [`ImageDecoder`](super::ImageDecoder) on the `image` crate: still formats
//! only (bmp, gif, ico, jpeg, png, tiff, webp), as the manifest's features
//! select. The crate's default decode limits apply.

use std::path::Path;

use super::{check_size, Decoded, ImageDecoder};

/// Decodes with the `image` crate.
pub struct ImageCrate;

impl ImageDecoder for ImageCrate {
    fn decode(&self, path: &Path, max_side: u32) -> Result<Decoded, String> {
        use image::ImageDecoder as _;
        let fail = |e: image::ImageError| e.to_string();
        let reader = image::ImageReader::open(path).map_err(|e| e.to_string())?;
        let mut decoder = reader.with_guessed_format().map_err(|e| e.to_string())?.into_decoder().map_err(fail)?;
        let orientation = decoder.orientation().map_err(fail)?;
        let mut img = image::DynamicImage::from_decoder(decoder).map_err(fail)?;
        img.apply_orientation(orientation);
        let source = (img.width(), img.height());
        check_size(source.0, source.1)?;
        if source.0 > max_side || source.1 > max_side {
            img = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
        }
        let rgba = img.to_rgba8();
        let size = (rgba.width(), rgba.height());
        Decoded::new(source, size, rgba.into_raw())
    }
}
