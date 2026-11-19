//! The pixel protocols' byte sequences: the kitty graphics transmit/place/delete commands,
//! the iTerm2 inline-image OSC, and a sixel encoder. Pure functions; the overlay painter
//! decides when each is written.

use std::fmt::Write as _;

use base64::Engine;

use super::raster::Rgba;

/// The kitty protocol's payload chunk limit (base64 bytes per APC).
const KITTY_CHUNK: usize = 4096;

/// Transmit a PNG under image `id` without displaying it (`a=t`), chunked; `q=2` keeps the
/// terminal from answering (an answer would arrive as input).
pub(crate) fn kitty_transmit(id: u32, png: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(KITTY_CHUNK).collect();
    let mut out = String::with_capacity(encoded.len() + chunks.len() * 32);
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let chunk = std::str::from_utf8(chunk).unwrap_or_default();
        if index == 0 {
            let _ = write!(out, "\x1b_Ga=t,f=100,t=d,i={id},q=2,m={more};{chunk}\x1b\\");
        } else {
            let _ = write!(out, "\x1b_Gm={more};{chunk}\x1b\\");
        }
    }
    out
}

/// Place image `id` at the cursor scaled to `cols` x `rows` cells. Placement 1 of an image is
/// replaced (moved) by the next placement with the same ids; `C=1` keeps the cursor put.
pub(crate) fn kitty_place(id: u32, cols: u16, rows: u16) -> String {
    format!("\x1b_Ga=p,i={id},p=1,c={cols},r={rows},C=1,q=2\x1b\\")
}

/// Remove image `id`'s placement, keeping its data for a later placement.
pub(crate) fn kitty_delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={id},p=1,q=2\x1b\\")
}

/// The iTerm2 inline image scaled to `cols` x `rows` cells at the cursor.
pub(crate) fn iterm2_inline(png: &[u8], cols: u16, rows: u16) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=0:{encoded}\x07",
        png.len()
    )
}

/// The sixel palette: a 6x6x6 colour cube (register = 36r + 6g + b).
fn cube_level(channel: u8) -> usize {
    (usize::from(channel) * 5 + 127) / 255
}

/// Encode `image` as one sixel DCS string at 1:1 pixel aspect. Transparent pixels (alpha
/// below half) are left unpainted (`P2 = 1`), so the cells beneath keep their colour.
pub(crate) fn sixel(image: &Rgba) -> String {
    let (width, height) = (image.width as usize, image.height as usize);
    let register = |x: usize, y: usize| -> Option<usize> {
        let [r, g, b, a] = image.pixels[y * width + x];
        (a >= 128).then(|| cube_level(r) * 36 + cube_level(g) * 6 + cube_level(b))
    };
    let mut used = [false; 216];
    for y in 0..height {
        for x in 0..width {
            if let Some(index) = register(x, y) {
                used[index] = true;
            }
        }
    }
    let mut out = format!("\x1bP0;1;0q\"1;1;{width};{height}");
    for (index, _) in used.iter().enumerate().filter(|(_, used)| **used) {
        let percent = |level: usize| level * 100 / 5;
        let _ = write!(
            out,
            "#{index};2;{};{};{}",
            percent(index / 36),
            percent(index / 6 % 6),
            percent(index % 6)
        );
    }
    let mut band_bits = vec![0u8; width];
    for band in 0..height.div_ceil(6) {
        let mut first_color = true;
        for (color, _) in used.iter().enumerate().filter(|(_, used)| **used) {
            let mut any = false;
            for (x, bits) in band_bits.iter_mut().enumerate() {
                *bits = 0;
                for bit in 0..6 {
                    let y = band * 6 + bit;
                    if y < height && register(x, y) == Some(color) {
                        *bits |= 1 << bit;
                        any = true;
                    }
                }
            }
            if !any {
                continue;
            }
            if !first_color {
                out.push('$');
            }
            first_color = false;
            let _ = write!(out, "#{color}");
            push_runs(&mut out, &band_bits);
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

/// Append one colour's band row run-length encoded (`!<count><char>` for repeats past 3).
fn push_runs(out: &mut String, bits: &[u8]) {
    let mut index = 0;
    while index < bits.len() {
        let value = bits[index];
        let run = bits[index..]
            .iter()
            .take_while(|&&next| next == value)
            .count();
        let glyph = char::from(0x3f + value);
        if run > 3 {
            let _ = write!(out, "!{run}{glyph}");
        } else {
            for _ in 0..run {
                out.push(glyph);
            }
        }
        index += run;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_transmits_in_chunks_and_places_by_id() {
        let png = vec![7u8; 4000];
        let transmit = kitty_transmit(9, &png);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
        assert_eq!(
            transmit,
            format!(
                "\x1b_Ga=t,f=100,t=d,i=9,q=2,m=1;{}\x1b\\\x1b_Gm=0;{}\x1b\\",
                &encoded[..4096],
                &encoded[4096..]
            )
        );
        assert_eq!(
            kitty_place(9, 12, 4),
            "\x1b_Ga=p,i=9,p=1,c=12,r=4,C=1,q=2\x1b\\"
        );
        assert_eq!(kitty_delete(9), "\x1b_Ga=d,d=i,i=9,p=1,q=2\x1b\\");
    }

    #[test]
    fn sixel_encodes_colour_registers_bands_and_runs() {
        // 5 x 7: red everywhere but one transparent pixel at (0, 6) - two bands, the second
        // holding only the bottom row.
        let mut pixels = vec![[255, 0, 0, 255]; 35];
        pixels[30] = [0, 0, 0, 0];
        let image = Rgba {
            width: 5,
            height: 7,
            pixels,
        };
        // Register 180 = (5, 0, 0): red. Band 1: all six rows set (`~`) five times; band 2:
        // bit 0 set for x = 1..5 only.
        assert_eq!(
            sixel(&image),
            "\x1bP0;1;0q\"1;1;5;7#180;2;100;0;0#180!5~-#180?!4@-\x1b\\"
        );
    }
}
