//! Decoding still images to RGBA pixels. Drawing them is `PaintCx::image`'s
//! job; this is the decode side.

use std::path::Path;

/// A decoded image: straight (not premultiplied) RGBA8 rows, top to bottom,
/// upright per the file's orientation tag.
pub struct Decoded {
    /// The size the file declares, after orientation.
    pub source_width: u32,
    pub source_height: u32,
    /// The size of `rgba`: the source size, or smaller if it was fitted.
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode the image at `path`, upright per its EXIF orientation. One larger
/// than `max_side` on either side is shrunk to fit, keeping its aspect ratio.
pub fn decode(path: &Path, max_side: u32) -> Result<Decoded, String> {
    use image::ImageDecoder;
    let fail = |e: image::ImageError| e.to_string();
    let reader = image::ImageReader::open(path).map_err(|e| e.to_string())?;
    let mut decoder = reader.with_guessed_format().map_err(|e| e.to_string())?.into_decoder().map_err(fail)?;
    let orientation = decoder.orientation().map_err(fail)?;
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(fail)?;
    img.apply_orientation(orientation);
    let (source_width, source_height) = (img.width(), img.height());
    if source_width == 0 || source_height == 0 {
        return Err("empty image".into());
    }
    if source_width > max_side || source_height > max_side {
        img = img.resize(max_side, max_side, image::imageops::FilterType::Triangle);
    }
    let rgba = img.to_rgba8();
    Ok(Decoded { source_width, source_height, width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}
