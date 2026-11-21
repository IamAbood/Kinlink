//! BGRA downscaling for the "Resolution" option (bilinear, multi-threaded).

/// Target size for a max height (0 = native), keeping aspect ratio; even dimensions for the encoder.
pub fn fit(w: usize, h: usize, max_h: u32) -> (usize, usize) {
    if max_h == 0 || h <= max_h as usize {
        return (w & !1, h & !1);
    }
    let nh = max_h as usize;
    let nw = (w * nh / h).max(2);
    (nw & !1, nh & !1)
}

pub fn bilinear(src: &[u8], sw: usize, sh: usize, dst: &mut Vec<u8>, dw: usize, dh: usize) {
    dst.resize(dw * dh * 4, 0);
    let (fx, fy) = (sw as f32 / dw as f32, sh as f32 / dh as f32);
    let rows_per = dh.div_ceil(8);
    std::thread::scope(|s| {
        for (band, out) in dst.chunks_mut(rows_per * dw * 4).enumerate() {
            s.spawn(move || {
                for (r, row) in out.chunks_mut(dw * 4).enumerate() {
                    let y = band * rows_per + r;
                    let sy = ((y as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
                    let (y0, ty) = (sy as usize, sy.fract());
                    let y1 = (y0 + 1).min(sh - 1);
                    for x in 0..dw {
                        let sx = ((x as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
                        let (x0, tx) = (sx as usize, sx.fract());
                        let x1 = (x0 + 1).min(sw - 1);
                        for c in 0..4 {
                            let p = |xx: usize, yy: usize| src[(yy * sw + xx) * 4 + c] as f32;
                            let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
                            let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
                            row[x * 4 + c] = (top + (bot - top) * ty + 0.5) as u8;
                        }
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_and_scale() {
        assert_eq!(fit(1920, 1080, 720), (1280, 720));
        assert_eq!(fit(1920, 1080, 0), (1920, 1080));
        assert_eq!(fit(1280, 720, 1080), (1280, 720));
        let src: Vec<u8> = (0..4 * 4).flat_map(|i| [i as u8 * 10, 50, 100, 255]).collect();
        let mut dst = Vec::new();
        bilinear(&src, 4, 4, &mut dst, 2, 2);
        assert_eq!(dst.len(), 16);
        assert_eq!(&dst[1..4], &[50, 100, 255]);
    }
}
