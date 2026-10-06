//! Artwork I/O and bounded image decoding, intended for a background worker.

use std::fs::File;
use std::io::{Cursor, Read};

use image::{ImageReader, Limits};

use crate::{ArtworkRef, Result, metadata};

pub const MAX_ARTWORK_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARTWORK_PIXELS: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkData {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Pixels are packed as `0xAARRGGBB`, which is BGRA on little-endian platforms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtworkBitmap {
    pub pixels: Vec<u32>,
    pub width: u32,
    pub height: u32,
}

/// Read local artwork. Remote/provider artwork must first be resolved and fetched
/// by the source provider, which can pass its bytes to [`decode_artwork`].
pub fn read_artwork(reference: &ArtworkRef, max_bytes: usize) -> Result<ArtworkData> {
    let max_bytes = max_bytes.min(MAX_ARTWORK_BYTES);
    match reference {
        ArtworkRef::File(path) => {
            if !std::fs::metadata(path)
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err("Artwork path must be a regular file".into());
            }
            let file = File::open(path).map_err(|error| error.to_string())?;
            if file.metadata().map_err(|error| error.to_string())?.len() > max_bytes as u64 {
                return Err("Artwork exceeds the compressed size limit".into());
            }
            let mut bytes = Vec::new();
            file.take(max_bytes as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            if bytes.len() > max_bytes {
                return Err("Artwork exceeds the compressed size limit".into());
            }
            let media_type = match image::guess_format(&bytes) {
                Ok(image::ImageFormat::Jpeg) => "image/jpeg",
                Ok(image::ImageFormat::Png) => "image/png",
                _ => return Err("Artwork must be a JPEG or PNG image".into()),
            };
            Ok(ArtworkData {
                bytes,
                media_type: media_type.into(),
            })
        }
        ArtworkRef::Embedded { path, index } => metadata::embedded_artwork(path, *index, max_bytes),
        ArtworkRef::Remote { .. } | ArtworkRef::Provider { .. } => {
            Err("Artwork requires resolution by its source provider".into())
        }
    }
}

/// Decode a JPEG/PNG and shrink it to at most 512 pixels per edge. Both the
/// compressed input and image dimensions are checked before full decoding.
pub fn decode_artwork(data: &ArtworkData, max_edge: u32) -> Result<ArtworkBitmap> {
    if data.bytes.len() > MAX_ARTWORK_BYTES {
        return Err("Artwork exceeds the compressed size limit".into());
    }
    if max_edge == 0 {
        return Err("Artwork edge size must be positive".into());
    }
    let make_reader = || -> Result<_> {
        let mut reader = ImageReader::new(Cursor::new(data.bytes.as_slice()))
            .with_guessed_format()
            .map_err(|error| error.to_string())?;
        if !matches!(
            reader.format(),
            Some(image::ImageFormat::Jpeg | image::ImageFormat::Png)
        ) {
            return Err("Artwork must be a JPEG or PNG image".into());
        }
        let mut limits = Limits::default();
        limits.max_image_width = Some(32_768);
        limits.max_image_height = Some(32_768);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        Ok(reader)
    };
    let (width, height) = make_reader()?
        .into_dimensions()
        .map_err(|error| error.to_string())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_ARTWORK_PIXELS {
        return Err("Artwork exceeds the decoded pixel limit".into());
    }
    let edge = max_edge.min(512);
    let decoded = make_reader()?.decode().map_err(|error| error.to_string())?;
    let rgba = if width > edge || height > edge {
        decoded.thumbnail(edge, edge).to_rgba8()
    } else {
        decoded.to_rgba8()
    };
    let pixels = rgba
        .pixels()
        .map(|pixel| {
            u32::from(pixel[3]) << 24
                | u32::from(pixel[0]) << 16
                | u32::from(pixel[1]) << 8
                | u32::from(pixel[2])
        })
        .collect();
    Ok(ArtworkBitmap {
        pixels,
        width: rgba.width(),
        height: rgba.height(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_decodes_to_bgra_and_resizes() {
        let image = image::RgbaImage::from_pixel(32, 16, image::Rgba([1, 2, 3, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let data = ArtworkData {
            bytes: bytes.into_inner(),
            media_type: "image/png".into(),
        };
        let result = decode_artwork(&data, 8).unwrap();
        assert_eq!((result.width, result.height), (8, 4));
        assert_eq!(result.pixels, vec![0xff01_0203; 32]);
    }

    #[test]
    fn malformed_and_oversize_inputs_are_rejected() {
        let bad = ArtworkData {
            bytes: b"bad image".to_vec(),
            media_type: "image/png".into(),
        };
        assert!(decode_artwork(&bad, 128).is_err());
        let huge = ArtworkData {
            bytes: vec![0; MAX_ARTWORK_BYTES + 1],
            media_type: "image/png".into(),
        };
        assert!(decode_artwork(&huge, 128).is_err());
        assert!(decode_artwork(&bad, 0).is_err());
    }

    #[test]
    fn huge_png_dimensions_are_rejected_before_decoding() {
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut buffer = Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        let mut bytes = buffer.into_inner();
        bytes[16..20].copy_from_slice(&6000_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&6000_u32.to_be_bytes());
        let mut crc = u32::MAX;
        for byte in &bytes[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        let data = ArtworkData {
            bytes,
            media_type: "image/png".into(),
        };
        assert!(
            decode_artwork(&data, 256)
                .unwrap_err()
                .contains("pixel limit")
        );
    }
}
