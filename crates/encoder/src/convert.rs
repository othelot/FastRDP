//! BGRA (IddCx swap-chain format B8G8R8A8) -> I420, BT.709 limited range, row-parallel.
//! CPU path used by the software encoders (x264 / OpenH264). Hardware encoders take BGRA directly.

/// Planar I420 buffer with tight strides.
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        I420 { width, height, y: vec![0; width * height], u: vec![128; cw * ch], v: vec![128; cw * ch] }
    }
    pub fn y_stride(&self) -> usize {
        self.width
    }
    pub fn c_stride(&self) -> usize {
        self.width.div_ceil(2)
    }
}

#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)) as u8
}

/// Converts `bgra` (stride in bytes) into `out`. `threads` = 0 -> available parallelism.
pub fn bgra_to_i420(bgra: &[u8], stride: usize, out: &mut I420, threads: usize) {
    let (w, h) = (out.width, out.height);
    assert!(stride >= w * 4 && bgra.len() >= stride * (h - 1) + w * 4, "BGRA buffer too small");
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let n = if threads == 0 { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) } else { threads };
    let n = n.clamp(1, ch.max(1));
    let rows_per = ch.div_ceil(n); // chroma rows per worker (each = 2 luma rows)

    let y_chunks = out.y.chunks_mut(rows_per * 2 * w);
    let u_chunks = out.u.chunks_mut(rows_per * cw);
    let v_chunks = out.v.chunks_mut(rows_per * cw);
    std::thread::scope(|s| {
        for (i, ((yc, uc), vc)) in y_chunks.zip(u_chunks).zip(v_chunks).enumerate() {
            s.spawn(move || {
                let cy0 = i * rows_per;
                let rows = uc.len() / cw;
                for cr in 0..rows {
                    let cy = cy0 + cr;
                    let y0 = cy * 2;
                    let y1 = (y0 + 1).min(h - 1);
                    let r0 = &bgra[y0 * stride..y0 * stride + w * 4];
                    let r1 = &bgra[y1 * stride..y1 * stride + w * 4];
                    let ly0 = cr * 2;
                    for cx in 0..cw {
                        let x0 = cx * 2;
                        let x1 = (x0 + 1).min(w - 1);
                        let mut sr = 0;
                        let mut sg = 0;
                        let mut sb = 0;
                        for (row, ly) in [(r0, ly0), (r1, ly0 + 1)] {
                            for x in [x0, x1] {
                                let p = &row[x * 4..x * 4 + 3];
                                let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
                                if ly * w + x < yc.len() && (ly == ly0 || y1 != y0) {
                                    yc[ly * w + x] = luma(r, g, b);
                                }
                                sr += r;
                                sg += g;
                                sb += b;
                            }
                        }
                        let (r, g, b) = ((sr + 2) >> 2, (sg + 2) >> 2, (sb + 2) >> 2);
                        uc[cr * cw + cx] = (128 + ((-26 * r - 87 * g + 112 * b + 128) >> 8)).clamp(0, 255) as u8;
                        vc[cr * cw + cx] = (128 + ((112 * r - 102 * g - 10 * b + 128) >> 8)).clamp(0, 255) as u8;
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
    fn known_colors_bt709_limited() {
        // white, black, pure red 2x2 blocks
        for (bgr, y, u, v) in [([255, 255, 255], 235, 128, 128), ([0, 0, 0], 16, 128, 128), ([0, 0, 255], 63, 102, 240)] {
            let mut px = Vec::new();
            for _ in 0..4 {
                px.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
            }
            let mut o = I420::new(2, 2);
            bgra_to_i420(&px, 8, &mut o, 1);
            assert!((o.y[0] as i32 - y).abs() <= 1, "y {} vs {}", o.y[0], y);
            assert!((o.u[0] as i32 - u).abs() <= 1, "u {} vs {}", o.u[0], u);
            assert!((o.v[0] as i32 - v).abs() <= 1, "v {} vs {}", o.v[0], v);
        }
    }

    #[test]
    fn threaded_equals_single() {
        let (w, h) = (64, 38);
        let px: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut a = I420::new(w, h);
        let mut b = I420::new(w, h);
        bgra_to_i420(&px, w * 4, &mut a, 1);
        bgra_to_i420(&px, w * 4, &mut b, 5);
        assert_eq!(a.y, b.y);
        assert_eq!(a.u, b.u);
        assert_eq!(a.v, b.v);
    }
}
