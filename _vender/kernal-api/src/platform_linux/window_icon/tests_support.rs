//! Minimal PNG writer, so icon tests carry no image fixtures.
//!
//! A committed `.png` would be an opaque blob a reader has to trust; a
//! generator makes the input to each test readable in the test itself, and
//! lets a test ask for a size (an oversized icon, say) without shipping a
//! megabyte to prove a bound.

/// Encode a solid-colour RGB PNG at `width` x `height`.
pub(super) fn rgb_png(width: u32, height: u32, colour: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let row: Vec<u8> = colour
            .iter()
            .copied()
            .cycle()
            .take(width as usize * 3)
            .collect();
        let mut data = Vec::with_capacity(row.len() * height as usize);
        for _ in 0..height {
            data.extend_from_slice(&row);
        }
        writer.write_image_data(&data).expect("png data");
    }
    out
}
