use crate::error::{Result, ScreenStreamError};

pub(crate) fn i420_plane_lengths(width: usize, height: usize) -> Result<(usize, usize)> {
    validate_even_dimensions("i420", width, height)?;
    let y_len = width
        .checked_mul(height)
        .ok_or_else(|| ScreenStreamError::InvalidFrame("i420 luma size overflows".into()))?;
    Ok((y_len, y_len / 4))
}

pub(crate) fn nv12_len(width: usize, height: usize) -> Result<usize> {
    validate_even_dimensions("nv12", width, height)?;
    let y_len = width
        .checked_mul(height)
        .ok_or_else(|| ScreenStreamError::InvalidFrame("nv12 luma size overflows".into()))?;
    y_len
        .checked_add(y_len / 2)
        .ok_or_else(|| ScreenStreamError::InvalidFrame("nv12 size overflows".into()))
}

pub(crate) fn fill_i420_from_bgra(
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    bgra: &[u8],
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
) -> Result<()> {
    let (y_len, uv_len) = i420_plane_lengths(width, height)?;
    validate_bgra_len(bgra, source_width, source_height)?;
    if y_plane.len() < y_len || u_plane.len() < uv_len || v_plane.len() < uv_len {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "i420 planes are too small for {width}x{height}"
        )));
    }

    if source_width == width && source_height == height {
        fill_i420_same_size(y_plane, u_plane, v_plane, bgra, width, height);
    } else {
        fill_i420_scaled(
            y_plane,
            u_plane,
            v_plane,
            bgra,
            source_width,
            source_height,
            width,
            height,
        );
    }

    Ok(())
}

pub(crate) fn fill_nv12_from_bgra(
    nv12: &mut [u8],
    bgra: &[u8],
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
) -> Result<()> {
    let total_len = nv12_len(width, height)?;
    validate_bgra_len(bgra, source_width, source_height)?;
    if nv12.len() < total_len {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "nv12 buffer has {} bytes, expected at least {total_len}",
            nv12.len()
        )));
    }

    if source_width == width && source_height == height {
        fill_nv12_same_size(nv12, bgra, width, height);
    } else {
        fill_nv12_scaled(nv12, bgra, source_width, source_height, width, height);
    }

    Ok(())
}

fn validate_even_dimensions(format: &str, width: usize, height: usize) -> Result<()> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "{format} encoder input must be positive and even, got {width}x{height}"
        )));
    }
    Ok(())
}

fn validate_bgra_len(bgra: &[u8], source_width: usize, source_height: usize) -> Result<()> {
    if source_width == 0 || source_height == 0 {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "invalid bgra source dimensions {source_width}x{source_height}"
        )));
    }
    let required_bgra = source_width
        .checked_mul(source_height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| ScreenStreamError::InvalidFrame("bgra frame size overflows".into()))?;

    if bgra.len() < required_bgra {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "bgra buffer has {} bytes, expected at least {required_bgra}",
            bgra.len()
        )));
    }
    Ok(())
}

fn fill_i420_same_size(
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    bgra: &[u8],
    width: usize,
    height: usize,
) {
    for y in (0..height).step_by(2) {
        let y0 = y * width;
        let y1 = y0 + width;
        let uv_row = (y / 2) * (width / 2);
        let src_row0 = y0 * 4;
        let src_row1 = y1 * 4;

        for x in (0..width).step_by(2) {
            let p00 = rgb_at_offset(bgra, src_row0 + x * 4);
            let p01 = rgb_at_offset(bgra, src_row0 + (x + 1) * 4);
            let p10 = rgb_at_offset(bgra, src_row1 + x * 4);
            let p11 = rgb_at_offset(bgra, src_row1 + (x + 1) * 4);

            y_plane[y0 + x] = rgb_to_y(p00.0, p00.1, p00.2);
            y_plane[y0 + x + 1] = rgb_to_y(p01.0, p01.1, p01.2);
            y_plane[y1 + x] = rgb_to_y(p10.0, p10.1, p10.2);
            y_plane[y1 + x + 1] = rgb_to_y(p11.0, p11.1, p11.2);

            let (r, g, b) = average_rgb_2x2(p00, p01, p10, p11);
            let uv = uv_row + x / 2;
            u_plane[uv] = rgb_to_u(r, g, b);
            v_plane[uv] = rgb_to_v(r, g, b);
        }
    }
}

fn fill_i420_scaled(
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
    bgra: &[u8],
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
) {
    for y in (0..height).step_by(2) {
        let y0 = y * width;
        let y1 = y0 + width;
        let uv_row = (y / 2) * (width / 2);
        let src_y0 = y * source_height / height;
        let src_y1 = (y + 1) * source_height / height;

        for x in (0..width).step_by(2) {
            let src_x0 = x * source_width / width;
            let src_x1 = (x + 1) * source_width / width;
            let p00 = rgb_at(bgra, source_width, src_x0, src_y0);
            let p01 = rgb_at(bgra, source_width, src_x1, src_y0);
            let p10 = rgb_at(bgra, source_width, src_x0, src_y1);
            let p11 = rgb_at(bgra, source_width, src_x1, src_y1);

            y_plane[y0 + x] = rgb_to_y(p00.0, p00.1, p00.2);
            y_plane[y0 + x + 1] = rgb_to_y(p01.0, p01.1, p01.2);
            y_plane[y1 + x] = rgb_to_y(p10.0, p10.1, p10.2);
            y_plane[y1 + x + 1] = rgb_to_y(p11.0, p11.1, p11.2);

            let (r, g, b) = average_rgb_2x2(p00, p01, p10, p11);
            let uv = uv_row + x / 2;
            u_plane[uv] = rgb_to_u(r, g, b);
            v_plane[uv] = rgb_to_v(r, g, b);
        }
    }
}

fn fill_nv12_same_size(nv12: &mut [u8], bgra: &[u8], width: usize, height: usize) {
    let y_len = width * height;
    let uv_offset = y_len;

    for y in (0..height).step_by(2) {
        let y0 = y * width;
        let y1 = y0 + width;
        let uv_row = uv_offset + (y / 2) * width;
        let src_row0 = y0 * 4;
        let src_row1 = y1 * 4;

        for x in (0..width).step_by(2) {
            let p00 = rgb_at_offset(bgra, src_row0 + x * 4);
            let p01 = rgb_at_offset(bgra, src_row0 + (x + 1) * 4);
            let p10 = rgb_at_offset(bgra, src_row1 + x * 4);
            let p11 = rgb_at_offset(bgra, src_row1 + (x + 1) * 4);

            nv12[y0 + x] = rgb_to_y(p00.0, p00.1, p00.2);
            nv12[y0 + x + 1] = rgb_to_y(p01.0, p01.1, p01.2);
            nv12[y1 + x] = rgb_to_y(p10.0, p10.1, p10.2);
            nv12[y1 + x + 1] = rgb_to_y(p11.0, p11.1, p11.2);

            let (r, g, b) = average_rgb_2x2(p00, p01, p10, p11);
            let uv = uv_row + x;
            nv12[uv] = rgb_to_u(r, g, b);
            nv12[uv + 1] = rgb_to_v(r, g, b);
        }
    }
}

fn fill_nv12_scaled(
    nv12: &mut [u8],
    bgra: &[u8],
    source_width: usize,
    source_height: usize,
    width: usize,
    height: usize,
) {
    let y_len = width * height;
    let uv_offset = y_len;

    for y in (0..height).step_by(2) {
        let y0 = y * width;
        let y1 = y0 + width;
        let uv_row = uv_offset + (y / 2) * width;
        let src_y0 = y * source_height / height;
        let src_y1 = (y + 1) * source_height / height;

        for x in (0..width).step_by(2) {
            let src_x0 = x * source_width / width;
            let src_x1 = (x + 1) * source_width / width;
            let p00 = rgb_at(bgra, source_width, src_x0, src_y0);
            let p01 = rgb_at(bgra, source_width, src_x1, src_y0);
            let p10 = rgb_at(bgra, source_width, src_x0, src_y1);
            let p11 = rgb_at(bgra, source_width, src_x1, src_y1);

            nv12[y0 + x] = rgb_to_y(p00.0, p00.1, p00.2);
            nv12[y0 + x + 1] = rgb_to_y(p01.0, p01.1, p01.2);
            nv12[y1 + x] = rgb_to_y(p10.0, p10.1, p10.2);
            nv12[y1 + x + 1] = rgb_to_y(p11.0, p11.1, p11.2);

            let (r, g, b) = average_rgb_2x2(p00, p01, p10, p11);
            let uv = uv_row + x;
            nv12[uv] = rgb_to_u(r, g, b);
            nv12[uv + 1] = rgb_to_v(r, g, b);
        }
    }
}

#[inline]
fn rgb_at(bgra: &[u8], source_width: usize, x: usize, y: usize) -> (u8, u8, u8) {
    rgb_at_offset(bgra, (y * source_width + x) * 4)
}

#[inline]
fn rgb_at_offset(bgra: &[u8], offset: usize) -> (u8, u8, u8) {
    (bgra[offset + 2], bgra[offset + 1], bgra[offset])
}

#[inline]
fn average_rgb_2x2(
    p00: (u8, u8, u8),
    p01: (u8, u8, u8),
    p10: (u8, u8, u8),
    p11: (u8, u8, u8),
) -> (u8, u8, u8) {
    (
        average4(p00.0, p01.0, p10.0, p11.0),
        average4(p00.1, p01.1, p10.1, p11.1),
        average4(p00.2, p01.2, p10.2, p11.2),
    )
}

#[inline]
fn average4(a: u8, b: u8, c: u8, d: u8) -> u8 {
    ((a as u32 + b as u32 + c as u32 + d as u32 + 2) / 4) as u8
}

#[inline]
fn rgb_to_y(r: u8, g: u8, b: u8) -> u8 {
    (((66 * r as i32 + 129 * g as i32 + 25 * b as i32 + 128) >> 8) + 16).clamp(0, 255) as u8
}

#[inline]
fn rgb_to_u(r: u8, g: u8, b: u8) -> u8 {
    (((-38 * r as i32 - 74 * g as i32 + 112 * b as i32 + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[inline]
fn rgb_to_v(r: u8, g: u8, b: u8) -> u8 {
    (((112 * r as i32 - 94 * g as i32 - 18 * b as i32 + 128) >> 8) + 128).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::{fill_i420_from_bgra, fill_nv12_from_bgra, rgb_to_y};

    #[test]
    fn i420_black_2x2_uses_limited_range_neutral_chroma() {
        let bgra = [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        let mut y = vec![0; 4];
        let mut u = vec![0; 1];
        let mut v = vec![0; 1];

        fill_i420_from_bgra(&mut y, &mut u, &mut v, &bgra, 2, 2, 2, 2).unwrap();

        assert_eq!(y, [16, 16, 16, 16]);
        assert_eq!(u, [128]);
        assert_eq!(v, [128]);
    }

    #[test]
    fn i420_red_2x2_keeps_expected_chroma() {
        let bgra = [
            0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ];
        let mut y = vec![0; 4];
        let mut u = vec![0; 1];
        let mut v = vec![0; 1];

        fill_i420_from_bgra(&mut y, &mut u, &mut v, &bgra, 2, 2, 2, 2).unwrap();

        assert_eq!(y, [82, 82, 82, 82]);
        assert_eq!(u, [90]);
        assert_eq!(v, [240]);
    }

    #[test]
    fn nv12_red_2x2_interleaves_chroma() {
        let bgra = [
            0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ];
        let mut nv12 = vec![0; 6];

        fill_nv12_from_bgra(&mut nv12, &bgra, 2, 2, 2, 2).unwrap();

        assert_eq!(nv12, [82, 82, 82, 82, 90, 240]);
    }

    #[test]
    fn scaled_4x4_to_2x2_uses_nearest_source_positions() {
        let mut bgra = vec![0; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                bgra[i] = 0;
                bgra[i + 1] = 0;
                bgra[i + 2] = (x + y * 4) as u8;
                bgra[i + 3] = 255;
            }
        }

        let mut nv12 = vec![0; 6];
        fill_nv12_from_bgra(&mut nv12, &bgra, 4, 4, 2, 2).unwrap();

        assert_eq!(
            &nv12[..4],
            &[
                rgb_to_y(0, 0, 0),
                rgb_to_y(2, 0, 0),
                rgb_to_y(8, 0, 0),
                rgb_to_y(10, 0, 0),
            ]
        );
    }
}
