//! Tiny dependency-free encoder for uncompressed 24-bit (BI_RGB) BMP files.

use std::io;
use std::path::Path;

use i_slint_core::graphics::Rgb8Pixel;

/// Encode `pixels` (row-major, top-to-bottom, `width * height` entries) as a
/// 24-bit BMP and write it to `path`.
pub fn write_bmp(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[Rgb8Pixel],
) -> io::Result<()> {
    debug_assert_eq!(pixels.len(), (width as usize) * (height as usize));

    // Each row is padded up to a multiple of 4 bytes.
    let row_stride = (width * 3).next_multiple_of(4);
    let pixel_data_size = row_stride * height;
    const HEADER_SIZE: u32 = 14 + 40; // BITMAPFILEHEADER + BITMAPINFOHEADER
    let file_size = HEADER_SIZE + pixel_data_size;

    let mut buf = Vec::with_capacity(file_size as usize);

    // --- BITMAPFILEHEADER (14 bytes) ---
    buf.extend_from_slice(b"BM");
    buf.extend_from_slice(&file_size.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
    buf.extend_from_slice(&HEADER_SIZE.to_le_bytes()); // pixel data offset

    // --- BITMAPINFOHEADER (40 bytes) ---
    buf.extend_from_slice(&40u32.to_le_bytes()); // header size
    buf.extend_from_slice(&(width as i32).to_le_bytes());
    // Positive height => rows are stored bottom-up.
    buf.extend_from_slice(&(height as i32).to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // planes
    buf.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    buf.extend_from_slice(&0u32.to_le_bytes()); // compression = BI_RGB
    buf.extend_from_slice(&pixel_data_size.to_le_bytes());
    buf.extend_from_slice(&2835i32.to_le_bytes()); // 72 DPI horizontal (px/m)
    buf.extend_from_slice(&2835i32.to_le_bytes()); // 72 DPI vertical (px/m)
    buf.extend_from_slice(&0u32.to_le_bytes()); // colors used
    buf.extend_from_slice(&0u32.to_le_bytes()); // important colors

    // --- Pixel data: bottom-up, BGR, padded rows ---
    let padding = (row_stride - width * 3) as usize;
    for y in (0..height as usize).rev() {
        let row = &pixels[y * width as usize..][..width as usize];
        for px in row {
            buf.push(px.b);
            buf.push(px.g);
            buf.push(px.r);
        }
        buf.resize(buf.len() + padding, 0);
    }

    std::fs::write(path, buf)
}
