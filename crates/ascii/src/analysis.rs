use crate::{
    Cell, EDGE_FALLING, EDGE_HORIZONTAL, EDGE_RISING, EDGE_VERTICAL, FILL_GLYPHS, Frame, Image,
};

/// Defaults follow the shader's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub kernel_radius: usize,
    pub sigma: f32,
    pub sigma_scale: f32,
    pub tau: f32,
    pub threshold: f32,
    /// Edge pixels a cell needs before it draws an edge, as a multiple of the
    /// cell's mean side length in pixels. The shader's fixed 8 of an 8x8 tile
    /// is 1.0 here.
    pub edge_threshold: f32,
    pub exposure: f32,
    pub attenuation: f32,
    pub invert: bool,
    pub edges: bool,
    pub fill: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            kernel_radius: 2,
            sigma: 2.0,
            sigma_scale: 1.6,
            tau: 1.0,
            threshold: 0.005,
            edge_threshold: 1.0,
            exposure: 1.0,
            attenuation: 1.0,
            invert: false,
            edges: true,
            fill: true,
        }
    }
}

const NO_EDGE: u8 = u8::MAX;

/// Holds the per-pixel passes for one image so it can be rendered at several
/// grid sizes without redoing them. Buffers are reused across images.
#[derive(Default)]
pub struct Analyzer {
    params: Params,
    width: usize,
    height: usize,
    luma: Vec<f32>,
    blur_a: Vec<f32>,
    blur_b: Vec<f32>,
    mask: Vec<u8>,
    sat_rgb: Vec<[u32; 3]>,
    sat_dir: Vec<[u32; 4]>,
}

impl Analyzer {
    pub fn new(params: Params) -> Self {
        Self {
            params,
            ..Default::default()
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, params: Params) {
        self.params = params;
    }

    pub fn analyze(&mut self, img: &Image) {
        let (w, h) = (img.width, img.height);
        self.width = w;
        self.height = h;
        let n = w * h;
        self.luma.resize(n, 0.0);
        self.blur_a.resize(n, 0.0);
        self.blur_b.resize(n, 0.0);
        self.mask.resize(n, 0);
        self.sat_rgb.resize((w + 1) * (h + 1), [0; 3]);
        self.sat_dir.resize((w + 1) * (h + 1), [0; 4]);

        for (l, px) in self.luma.iter_mut().zip(img.rgb.chunks_exact(3)) {
            *l = luma(px[0], px[1], px[2]) / 255.0;
        }
        self.difference_of_gaussians();
        self.integrate(img);
    }

    fn difference_of_gaussians(&mut self) {
        let (w, h) = (self.width, self.height);
        let p = self.params;
        let r = p.kernel_radius as isize;
        let ka = kernel(p.sigma, p.kernel_radius);
        let kb = kernel(p.sigma * p.sigma_scale, p.kernel_radius);

        for y in 0..h {
            let row = &self.luma[y * w..(y + 1) * w];
            for x in 0..w {
                let (mut a, mut b) = (0.0, 0.0);
                for k in -r..=r {
                    let v = row[(x as isize + k).clamp(0, w as isize - 1) as usize];
                    a += v * ka[(k + r) as usize];
                    b += v * kb[(k + r) as usize];
                }
                self.blur_a[y * w + x] = a;
                self.blur_b[y * w + x] = b;
            }
        }

        for y in 0..h {
            for x in 0..w {
                let (mut a, mut b) = (0.0, 0.0);
                for k in -r..=r {
                    let yy = (y as isize + k).clamp(0, h as isize - 1) as usize;
                    a += self.blur_a[yy * w + x] * ka[(k + r) as usize];
                    b += self.blur_b[yy * w + x] * kb[(k + r) as usize];
                }
                self.mask[y * w + x] = (a - p.tau * b >= p.threshold) as u8;
            }
        }
    }

    /// Builds summed-area tables of color and of edge-direction counts, so
    /// any cell's average color and direction histogram is four lookups.
    fn integrate(&mut self, img: &Image) {
        let (w, h) = (self.width, self.height);
        let stride = w + 1;
        self.sat_rgb[..stride].fill([0; 3]);
        self.sat_dir[..stride].fill([0; 4]);
        for y in 0..h {
            let mut row_rgb = [0u32; 3];
            let mut row_dir = [0u32; 4];
            self.sat_rgb[(y + 1) * stride] = [0; 3];
            self.sat_dir[(y + 1) * stride] = [0; 4];
            for x in 0..w {
                let px = &img.rgb[(y * w + x) * 3..][..3];
                for c in 0..3 {
                    row_rgb[c] += px[c] as u32;
                }
                let d = self.direction(x, y);
                if d != NO_EDGE {
                    row_dir[d as usize] += 1;
                }
                let above_rgb = self.sat_rgb[y * stride + x + 1];
                let above_dir = self.sat_dir[y * stride + x + 1];
                let i = (y + 1) * stride + x + 1;
                self.sat_rgb[i] = std::array::from_fn(|c| above_rgb[c] + row_rgb[c]);
                self.sat_dir[i] = std::array::from_fn(|c| above_dir[c] + row_dir[c]);
            }
        }
    }

    /// Sobel over the DoG mask, with the shader's 3-10-3 weights. Returns an
    /// index into [vertical, horizontal, rising, falling], or `NO_EDGE`.
    fn direction(&self, x: usize, y: usize) -> u8 {
        let (w, h) = (self.width, self.height);
        let at = |dx: isize, dy: isize| -> i32 {
            let xx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
            let yy = (y as isize + dy).clamp(0, h as isize - 1) as usize;
            self.mask[yy * w + xx] as i32
        };
        let gx =
            3 * (at(1, -1) - at(-1, -1)) + 10 * (at(1, 0) - at(-1, 0)) + 3 * (at(1, 1) - at(-1, 1));
        let gy =
            3 * (at(-1, 1) - at(-1, -1)) + 10 * (at(0, 1) - at(0, -1)) + 3 * (at(1, 1) - at(1, -1));
        if gx == 0 && gy == 0 {
            return NO_EDGE;
        }
        classify(gx as f32, gy as f32)
    }

    pub fn render(&self, cols: u16, rows: u16) -> Frame {
        let mut frame = Frame::blank(cols, rows);
        if self.width == 0 || self.height == 0 || cols == 0 || rows == 0 {
            return frame;
        }
        let p = &self.params;
        let (w, h) = (self.width, self.height);
        let stride = w + 1;
        for r in 0..rows as usize {
            let y0 = r * h / rows as usize;
            let y1 = ((r + 1) * h / rows as usize).max(y0 + 1).min(h);
            for c in 0..cols as usize {
                let x0 = c * w / cols as usize;
                let x1 = ((c + 1) * w / cols as usize).max(x0 + 1).min(w);
                let area = ((x1 - x0) * (y1 - y0)) as u32;

                let sum_rgb = rect_sum(&self.sat_rgb, stride, x0, y0, x1, y1);
                let rgb: [u8; 3] = std::array::from_fn(|i| ((sum_rgb[i] + area / 2) / area) as u8);

                let glyph = self
                    .edge_glyph(x0, y0, x1, y1)
                    .or_else(|| p.fill.then(|| fill_glyph(rgb, p)))
                    .unwrap_or(0);
                frame.cells[r * cols as usize + c] = Cell { glyph, rgb };
            }
        }
        frame
    }

    fn edge_glyph(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> Option<u8> {
        if !self.params.edges {
            return None;
        }
        let counts = rect_sum(&self.sat_dir, self.width + 1, x0, y0, x1, y1);
        let (best, &count) = counts
            .iter()
            .enumerate()
            .max_by_key(|&(i, n)| (n, std::cmp::Reverse(i)))?;
        let side = ((x1 - x0) + (y1 - y0)) as f32 / 2.0;
        if (count as f32) < self.params.edge_threshold * side || count == 0 {
            return None;
        }
        Some([EDGE_VERTICAL, EDGE_HORIZONTAL, EDGE_RISING, EDGE_FALLING][best])
    }
}

fn luma(r: u8, g: u8, b: u8) -> f32 {
    0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32
}

fn kernel(sigma: f32, radius: usize) -> Vec<f32> {
    let r = radius as isize;
    let k: Vec<f32> = (-r..=r)
        .map(|x| (-(x * x) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = k.iter().sum();
    k.into_iter().map(|v| v / sum).collect()
}

/// Glyphs are drawn in cells about twice as tall as wide, so '/' and '\' sit
/// near 63 degrees, not 45. The bins split halfway between the glyph angles
/// measured on the gradient (the edge normal): '|' at 0, '/' or '\' at 26.6,
/// '-' at 90.
fn classify(gx: f32, gy: f32) -> u8 {
    const TAN_VERTICAL_LIMIT: f32 = 0.2364; // tan(13.3 deg)
    const TAN_HORIZONTAL_LIMIT: f32 = 1.6198; // tan(58.3 deg)
    let (ax, ay) = (gx.abs(), gy.abs());
    if ay < TAN_VERTICAL_LIMIT * ax {
        0
    } else if ay > TAN_HORIZONTAL_LIMIT * ax {
        1
    } else if (gx > 0.0) == (gy > 0.0) {
        2
    } else {
        3
    }
}

fn fill_glyph(rgb: [u8; 3], p: &Params) -> u8 {
    let mut l = (luma(rgb[0], rgb[1], rgb[2]) / 255.0 * p.exposure)
        .powf(p.attenuation)
        .clamp(0.0, 1.0);
    if p.invert {
        l = 1.0 - l;
    }
    let level = (l * FILL_GLYPHS as f32).floor() - 1.0;
    level.clamp(0.0, FILL_GLYPHS as f32 - 1.0) as u8
}

fn rect_sum<const N: usize>(
    sat: &[[u32; N]],
    stride: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> [u32; N] {
    let (a, b, c, d) = (
        sat[y0 * stride + x0],
        sat[y0 * stride + x1],
        sat[y1 * stride + x0],
        sat[y1 * stride + x1],
    );
    std::array::from_fn(|i| d[i] + a[i] - b[i] - c[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Image {
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let v = f(x, y);
                rgb.extend([v, v, v]);
            }
        }
        Image::new(w, h, rgb)
    }

    fn render(img: &Image, cols: u16, rows: u16) -> Frame {
        let mut a = Analyzer::new(Params::default());
        a.analyze(img);
        a.render(cols, rows)
    }

    fn glyph_count(frame: &Frame, glyph: u8) -> usize {
        frame.cells.iter().filter(|c| c.glyph == glyph).count()
    }

    #[test]
    fn flat_images_fill_by_luminance() {
        let black = render(&image(64, 64, |_, _| 0), 8, 4);
        assert!(black.cells.iter().all(|c| c.glyph == 0));
        let white = render(&image(64, 64, |_, _| 255), 8, 4);
        assert!(
            white
                .cells
                .iter()
                .all(|c| c.glyph == 9 && c.rgb == [255; 3])
        );
        let mid = render(&image(64, 64, |_, _| 140), 8, 4);
        assert!(mid.cells.iter().all(|c| c.glyph == 4));
    }

    #[test]
    fn vertical_boundary_draws_pipes() {
        let f = render(
            &image(160, 160, |x, _| if x < 80 { 20 } else { 230 }),
            20,
            10,
        );
        assert!(
            glyph_count(&f, EDGE_VERTICAL) >= 8,
            "{:#?}",
            f.lines().collect::<Vec<_>>()
        );
        assert_eq!(glyph_count(&f, EDGE_HORIZONTAL), 0);
    }

    #[test]
    fn horizontal_boundary_draws_dashes() {
        let f = render(
            &image(160, 160, |_, y| if y < 80 { 20 } else { 230 }),
            20,
            10,
        );
        assert!(
            glyph_count(&f, EDGE_HORIZONTAL) >= 16,
            "{:#?}",
            f.lines().collect::<Vec<_>>()
        );
        assert_eq!(glyph_count(&f, EDGE_VERTICAL), 0);
    }

    #[test]
    fn diagonals_follow_glyph_slant() {
        // A boundary along y = h - 2x, which is the slope of '/' in a 1:2 cell.
        let rising = render(
            &image(160, 320, |x, y| if y + 2 * x < 320 { 20 } else { 230 }),
            20,
            20,
        );
        assert!(
            glyph_count(&rising, EDGE_RISING) >= 10,
            "{:#?}",
            rising.lines().collect::<Vec<_>>()
        );
        assert_eq!(glyph_count(&rising, EDGE_FALLING), 0);

        let falling = render(
            &image(160, 320, |x, y| if y < 2 * x { 20 } else { 230 }),
            20,
            20,
        );
        assert!(
            glyph_count(&falling, EDGE_FALLING) >= 10,
            "{:#?}",
            falling.lines().collect::<Vec<_>>()
        );
        assert_eq!(glyph_count(&falling, EDGE_RISING), 0);
    }

    #[test]
    fn renders_any_grid_size() {
        let img = image(100, 37, |x, y| ((x * 7 + y * 13) % 256) as u8);
        let mut a = Analyzer::new(Params::default());
        a.analyze(&img);
        for (cols, rows) in [(1, 1), (7, 3), (100, 37), (150, 60)] {
            let f = a.render(cols, rows);
            assert_eq!(f.cells.len(), cols as usize * rows as usize);
        }
    }
}
