//! Windows clipboard bitmaps to PNG: the `CF_DIB`/`CF_DIBV5` payload (a `BITMAPINFOHEADER`,
//! `BITMAPV4HEADER` or `BITMAPV5HEADER`, optional colour masks and palette, then the pixel rows)
//! decoded to RGBA and re-encoded as a PNG the providers accept. The decode is platform-neutral so
//! it is tested everywhere; only the clipboard read is Windows-only.

use std::io::Write;

/// `wingdi.h` `BI_RGB`.
const BI_RGB: u32 = 0;
/// `wingdi.h` `BI_BITFIELDS`.
const BI_BITFIELDS: u32 = 3;
/// `wingdi.h` `BI_ALPHABITFIELDS`.
const BI_ALPHABITFIELDS: u32 = 6;
/// The `BITMAPINFOHEADER` size; V4 (108) and V5 (124) extend it.
const INFO_HEADER_SIZE: usize = 40;
/// Where V4/V5 headers carry the red, green, blue and alpha masks.
const HEADER_MASKS_OFFSET: usize = 40;
/// Width and height bound for one pasted bitmap: larger than any display, small enough that a
/// corrupt header cannot ask for an unbounded allocation.
const MAX_DIMENSION: u32 = 32_768;

/// Why a clipboard bitmap could not be converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitmapError {
    /// The payload ends before the header, palette, or pixel rows it declares.
    Truncated,
    /// A header, compression, or bit depth this decoder does not read (JPEG/PNG-in-DIB, RLE).
    Unsupported,
}

/// One channel's mask: where it sits and how wide it is.
#[derive(Debug, Clone, Copy)]
struct Channel {
    mask: u32,
    shift: u32,
    bits: u32,
}

impl Channel {
    fn new(mask: u32) -> Self {
        Self {
            mask,
            shift: mask.trailing_zeros().min(31),
            bits: mask.count_ones(),
        }
    }

    /// The channel's value in `pixel`, scaled to 8 bits; `None` for an absent channel.
    fn read(self, pixel: u32) -> Option<u8> {
        if self.mask == 0 {
            return None;
        }
        let value = (pixel & self.mask) >> self.shift;
        let max = (1u64 << self.bits) - 1;
        u8::try_from(u64::from(value) * 255 / max).ok()
    }
}

/// Little-endian reads that fail as [`BitmapError::Truncated`].
fn read_u16(bytes: &[u8], at: usize) -> Result<u16, BitmapError> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or(BitmapError::Truncated)
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, BitmapError> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(BitmapError::Truncated)
}

/// A decoded bitmap: straight (non-premultiplied) RGBA rows, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rgba {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

/// Decode a packed DIB (the `CF_DIB`/`CF_DIBV5` clipboard payload) to RGBA. 32-bit `BI_RGB`
/// rows whose alpha bytes are all zero are opaque (GDI leaves the fourth byte unused).
pub(crate) fn decode_dib(dib: &[u8]) -> Result<Rgba, BitmapError> {
    let header_size = read_u32(dib, 0)? as usize;
    if header_size < INFO_HEADER_SIZE {
        // BITMAPCOREHEADER (OS/2) payloads never reach the Windows clipboard formats read here.
        return Err(BitmapError::Unsupported);
    }
    let width = i32::from_le_bytes(read_u32(dib, 4)?.to_le_bytes());
    let raw_height = i32::from_le_bytes(read_u32(dib, 8)?.to_le_bytes());
    let bit_count = read_u16(dib, 14)?;
    let compression = read_u32(dib, 16)?;
    let colors_used = read_u32(dib, 32)?;
    let top_down = raw_height < 0;
    let width = u32::try_from(width).map_err(|_| BitmapError::Unsupported)?;
    let height = raw_height.unsigned_abs();
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(BitmapError::Unsupported);
    }

    // The colour masks: inside a V4/V5 header, after a plain info header otherwise.
    let mut cursor = header_size;
    let masks = match compression {
        BI_BITFIELDS | BI_ALPHABITFIELDS => {
            let count = if compression == BI_ALPHABITFIELDS {
                4
            } else {
                3
            };
            let base = if header_size > INFO_HEADER_SIZE {
                HEADER_MASKS_OFFSET
            } else {
                cursor += count * 4;
                header_size
            };
            let alpha = if count == 4 || header_size >= HEADER_MASKS_OFFSET + 16 {
                read_u32(dib, base + 12)?
            } else {
                0
            };
            Some([
                read_u32(dib, base)?,
                read_u32(dib, base + 4)?,
                read_u32(dib, base + 8)?,
                alpha,
            ])
        }
        BI_RGB => None,
        _ => return Err(BitmapError::Unsupported),
    };
    let palette_len = match bit_count {
        1 | 4 | 8 => {
            let max = 1usize << bit_count;
            match colors_used as usize {
                0 => max,
                used => used.min(max),
            }
        }
        16 | 24 | 32 => 0,
        _ => return Err(BitmapError::Unsupported),
    };
    let palette = dib
        .get(cursor..cursor + palette_len * 4)
        .ok_or(BitmapError::Truncated)?;
    cursor += palette_len * 4;

    let stride = (width as usize * usize::from(bit_count)).div_ceil(32) * 4;
    let rows = dib
        .get(cursor..cursor + stride * height as usize)
        .ok_or(BitmapError::Truncated)?;
    let channels = match (bit_count, masks) {
        (16 | 32, Some([r, g, b, a])) => Some([r, g, b, a].map(Channel::new)),
        // 16-bit BI_RGB is 5-5-5.
        (16, None) => Some([0x7c00, 0x03e0, 0x001f, 0].map(Channel::new)),
        _ => None,
    };

    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let source_row = if top_down { y } else { height as usize - 1 - y };
        let row = &rows[source_row * stride..(source_row + 1) * stride];
        for x in 0..width as usize {
            let rgba = match (bit_count, channels) {
                (1 | 4 | 8, _) => {
                    let bits = usize::from(bit_count);
                    let bit = x * bits;
                    let byte = row[bit / 8];
                    let index = (byte >> (8 - bits - bit % 8)) & (u8::MAX >> (8 - bits));
                    let entry = palette
                        .get(usize::from(index) * 4..usize::from(index) * 4 + 4)
                        .ok_or(BitmapError::Truncated)?;
                    [entry[2], entry[1], entry[0], 255]
                }
                (24, _) => [row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255],
                (32, None) => [row[x * 4 + 2], row[x * 4 + 1], row[x * 4], row[x * 4 + 3]],
                (16 | 32, Some([r, g, b, a])) => {
                    let pixel = if bit_count == 16 {
                        u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]))
                    } else {
                        u32::from_le_bytes([
                            row[x * 4],
                            row[x * 4 + 1],
                            row[x * 4 + 2],
                            row[x * 4 + 3],
                        ])
                    };
                    [
                        r.read(pixel).unwrap_or(0),
                        g.read(pixel).unwrap_or(0),
                        b.read(pixel).unwrap_or(0),
                        a.read(pixel).unwrap_or(255),
                    ]
                }
                _ => return Err(BitmapError::Unsupported),
            };
            pixels.extend_from_slice(&rgba);
        }
    }
    // An all-zero alpha channel is GDI's unused fourth byte, not a fully transparent image.
    let alpha_carried = bit_count == 32
        && masks.is_none_or(|[_, _, _, alpha]| alpha != 0)
        && pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0);
    if bit_count == 32 && !alpha_carried {
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    Ok(Rgba {
        width,
        height,
        pixels,
    })
}

/// One PNG chunk: length, type, data, CRC over type and data.
fn write_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let length = u32::try_from(data.len()).expect("a PNG chunk under 4 GiB");
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(&kind);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// Encode RGBA rows as a PNG: RGB when every pixel is opaque, RGBA otherwise; filter type 0 per
/// row, zlib-compressed.
pub(crate) fn encode_png(image: &Rgba) -> Vec<u8> {
    let opaque = image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .all(|pixel| pixel[3] == 255);
    let (color_type, channels) = if opaque { (2u8, 3usize) } else { (6u8, 4usize) };
    let mut raw = Vec::with_capacity((image.width as usize * channels + 1) * image.height as usize);
    for row in image.pixels.chunks_exact(image.width as usize * 4) {
        raw.push(0);
        if opaque {
            for pixel in row.as_chunks::<4>().0 {
                raw.extend_from_slice(&pixel[..3]);
            }
        } else {
            raw.extend_from_slice(row);
        }
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&raw)
        .expect("compressing into memory cannot fail");
    let compressed = encoder
        .finish()
        .expect("compressing into memory cannot fail");

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&image.width.to_be_bytes());
    header.extend_from_slice(&image.height.to_be_bytes());
    // Bit depth 8, the colour type, deflate, adaptive filtering, no interlace.
    header.extend_from_slice(&[8, color_type, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    write_chunk(&mut png, *b"IHDR", &header);
    write_chunk(&mut png, *b"IDAT", &compressed);
    write_chunk(&mut png, *b"IEND", &[]);
    png
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A packed DIB: `header_size` bytes of header (fields after the info header zeroed unless
    /// `v5_masks`), then `extra` (masks or palette), then `rows`.
    fn dib(
        header_size: u32,
        size: (i32, i32),
        bit_count: u16,
        compression: u32,
        v5_masks: Option<[u32; 4]>,
        extra: &[u8],
        rows: &[u8],
    ) -> Vec<u8> {
        let mut out = vec![0u8; header_size as usize];
        out[0..4].copy_from_slice(&header_size.to_le_bytes());
        out[4..8].copy_from_slice(&size.0.to_le_bytes());
        out[8..12].copy_from_slice(&size.1.to_le_bytes());
        out[12..14].copy_from_slice(&1u16.to_le_bytes());
        out[14..16].copy_from_slice(&bit_count.to_le_bytes());
        out[16..20].copy_from_slice(&compression.to_le_bytes());
        if let Some(masks) = v5_masks {
            for (index, mask) in masks.iter().enumerate() {
                let at = HEADER_MASKS_OFFSET + index * 4;
                out[at..at + 4].copy_from_slice(&mask.to_le_bytes());
            }
        }
        out.extend_from_slice(extra);
        out.extend_from_slice(rows);
        out
    }

    /// A bottom-up 24-bit DIB (rows padded to 4 bytes) decodes top row first.
    #[test]
    fn a_bottom_up_24_bit_dib_decodes_top_row_first() {
        // 2x2: bottom row blue, white; top row red, green. BGR order, 2 bytes of row padding.
        let rows = [
            255, 0, 0, 255, 255, 255, 0, 0, //
            0, 0, 255, 0, 255, 0, 0, 0,
        ];
        let decoded = decode_dib(&dib(40, (2, 2), 24, BI_RGB, None, &[], &rows));
        assert_eq!(
            decoded,
            Ok(Rgba {
                width: 2,
                height: 2,
                pixels: vec![
                    255, 0, 0, 255, 0, 255, 0, 255, //
                    0, 0, 255, 255, 255, 255, 255, 255,
                ],
            })
        );
    }

    /// GDI leaves 32-bit `BI_RGB` alpha bytes zero: the image is opaque, not invisible.
    #[test]
    fn a_32_bit_dib_with_unused_alpha_is_opaque() {
        let rows = [10, 20, 30, 0];
        let decoded = decode_dib(&dib(40, (1, -1), 32, BI_RGB, None, &[], &rows));
        assert_eq!(
            decoded,
            Ok(Rgba {
                width: 1,
                height: 1,
                pixels: vec![30, 20, 10, 255],
            })
        );
    }

    /// A `CF_DIBV5` screenshot: masks inside the V5 header, a real alpha channel kept.
    #[test]
    fn a_v5_bitfields_dib_keeps_its_alpha() {
        let masks = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000];
        let rows = [1, 2, 3, 128, 4, 5, 6, 0];
        let decoded = decode_dib(&dib(
            124,
            (2, -1),
            32,
            BI_BITFIELDS,
            Some(masks),
            &[],
            &rows,
        ));
        assert_eq!(
            decoded,
            Ok(Rgba {
                width: 2,
                height: 1,
                pixels: vec![3, 2, 1, 128, 6, 5, 4, 0],
            })
        );
    }

    /// A plain info header with `BI_BITFIELDS` carries its masks after the header (RGB 565).
    #[test]
    fn a_565_dib_reads_masks_after_the_info_header() {
        let mut masks = Vec::new();
        for mask in [0xf800u32, 0x07e0, 0x001f] {
            masks.extend_from_slice(&mask.to_le_bytes());
        }
        // One pure-red pixel, padded to 4 bytes.
        let rows = [0x00, 0xf8, 0, 0];
        let decoded = decode_dib(&dib(40, (1, 1), 16, BI_BITFIELDS, None, &masks, &rows));
        assert_eq!(
            decoded,
            Ok(Rgba {
                width: 1,
                height: 1,
                pixels: vec![255, 0, 0, 255],
            })
        );
    }

    /// An 8-bit paletted DIB maps indices through its BGRX palette.
    #[test]
    fn an_8_bit_dib_maps_its_palette() {
        let mut header = dib(40, (2, 1), 8, BI_RGB, None, &[], &[]);
        header[32..36].copy_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&[0, 0, 0, 0, 255, 128, 0, 0]);
        header.extend_from_slice(&[1, 0, 0, 0]);
        assert_eq!(
            decode_dib(&header),
            Ok(Rgba {
                width: 2,
                height: 1,
                pixels: vec![0, 128, 255, 255, 0, 0, 0, 255],
            })
        );
    }

    #[test]
    fn truncated_and_compressed_dibs_are_refused() {
        let short = dib(40, (4, 4), 24, BI_RGB, None, &[], &[0; 8]);
        assert_eq!(decode_dib(&short), Err(BitmapError::Truncated));
        // BI_JPEG (4) payloads are not decoded.
        let jpeg = dib(40, (1, 1), 24, 4, None, &[], &[0; 4]);
        assert_eq!(decode_dib(&jpeg), Err(BitmapError::Unsupported));
        assert_eq!(decode_dib(&[0; 3]), Err(BitmapError::Truncated));
    }

    /// The PNG round-trips: the signature, an IHDR naming the size and colour type, and an IDAT
    /// that inflates back to the filtered rows.
    #[test]
    fn the_png_encoder_writes_inflatable_rows() {
        let image = Rgba {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 255, 4, 5, 6, 128],
        };
        let png = encode_png(&image);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..29], &[0, 0, 0, 2, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        let idat_length = u32::from_be_bytes([png[33], png[34], png[35], png[36]]) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let mut rows = Vec::new();
        flate2::read::ZlibDecoder::new(&png[41..41 + idat_length])
            .read_to_end(&mut rows)
            .expect("inflate");
        assert_eq!(rows, vec![0, 1, 2, 3, 255, 4, 5, 6, 128]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert_eq!(
            crate::image_load::detect_supported_image_mime_from_bytes(&png),
            Some("image/png")
        );
    }
}
