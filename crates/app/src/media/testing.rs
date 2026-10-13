//! Fixtures for tests of code that decodes or plays media, so those tests don't
//! name the codec crates either.

use std::io::Cursor;

fn encode(w: u32, h: u32, rgba: [u8; 4], format: image::ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(w, h, image::Rgba(rgba)).write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// A PNG of `w` x `h` pixels of one colour.
pub(crate) fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    encode(w, h, rgba, image::ImageFormat::Png)
}

/// A GIF of `w` x `h` pixels of one colour.
pub(crate) fn gif(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    encode(w, h, rgba, image::ImageFormat::Gif)
}

/// A mono 16-bit PCM WAV of `frames` silent frames.
pub(crate) fn wav(rate: u32, frames: u32) -> Vec<u8> {
    let data = frames * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data.to_le_bytes());
    v.resize(v.len() + data as usize, 0);
    v
}
