//! Decoding and playing media files, behind types and traits of our own so no
//! other code names the crates that do the work. The file-browser preview uses
//! it now; anything else that shows an image or plays a sound should too.

pub mod audio;
pub mod image;
