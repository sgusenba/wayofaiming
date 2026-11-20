//! Locating the black aiming mark in a grayscale camera frame.
//!
//! The camera is mounted on the barrel and looks at the target, so the
//! optical axis (image centre) is the aim point. Finding the black mark
//! gives both its centre and its size in pixels; the known physical size of
//! the mark converts pixels to millimetres.
//!
//! The black contains white ring lines, which split it into a disc and
//! several annuli. Every one of those pieces is centred on the target centre,
//! so the largest roughly circular dark component is used: its centroid is
//! the target centre and its bounding box gives the outer radius.

use crate::target::TargetSpec;

/// A borrowed 8-bit grayscale image, row-major, `width * height` bytes.
#[derive(Debug, Clone, Copy)]
pub struct GrayImage<'a> {
    pub width: usize,
    pub height: usize,
    pub data: &'a [u8],
}

/// The aiming mark as found in the image (pixel units).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkDetection {
    pub center: [f64; 2],
    pub radius: f64,
    /// Dark-pixel threshold that was used.
    pub threshold: u8,
}

#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Smallest accepted mark radius in pixels. Below this the mm/px scale
    /// is too coarse to be useful.
    pub min_radius_px: f64,
    /// Accepted bounding-box aspect ratio deviation from 1.
    pub max_aspect_deviation: f64,
    /// Minimum `area / (pi * r^2)`; annuli of the black are well above this.
    pub min_fill: f64,
    /// Fixed dark threshold; `None` selects one automatically (Otsu).
    pub threshold: Option<u8>,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            min_radius_px: 8.0,
            max_aspect_deviation: 0.25,
            min_fill: 0.15,
            threshold: None,
        }
    }
}

/// Otsu's threshold on the image histogram.
pub fn otsu_threshold(img: &GrayImage) -> u8 {
    let mut hist = [0u64; 256];
    for &p in img.data {
        hist[p as usize] += 1;
    }
    let total = img.data.len() as f64;
    let sum_all: f64 = hist.iter().enumerate().map(|(i, &c)| i as f64 * c as f64).sum();
    let (mut w_bg, mut sum_bg, mut best, mut best_t) = (0.0, 0.0, -1.0, 0u8);
    for (t, &count) in hist.iter().enumerate() {
        w_bg += count as f64;
        if w_bg == 0.0 {
            continue;
        }
        let w_fg = total - w_bg;
        if w_fg == 0.0 {
            break;
        }
        sum_bg += t as f64 * count as f64;
        let mean_bg = sum_bg / w_bg;
        let mean_fg = (sum_all - sum_bg) / w_fg;
        let between = w_bg * w_fg * (mean_bg - mean_fg).powi(2);
        if between > best {
            best = between;
            best_t = t as u8;
        }
    }
    best_t
}

#[derive(Debug, Default, Clone, Copy)]
struct Component {
    area: u64,
    sum_x: u64,
    sum_y: u64,
    min_x: usize,
    max_x: usize,
    min_y: usize,
    max_y: usize,
    touches_border: bool,
}

/// Find the black aiming mark. Returns `None` if no plausible mark is visible.
pub fn find_mark(img: &GrayImage, cfg: &DetectorConfig) -> Option<MarkDetection> {
    let (w, h) = (img.width, img.height);
    assert_eq!(img.data.len(), w * h, "image buffer size mismatch");
    if w < 3 || h < 3 {
        return None;
    }
    let threshold = cfg.threshold.unwrap_or_else(|| otsu_threshold(img));

    // Connected components (4-connectivity) of dark pixels via iterative flood fill.
    let mut label = vec![u32::MAX; w * h];
    let mut stack: Vec<usize> = Vec::new();
    let mut best: Option<(f64, Component)> = None;

    for start in 0..w * h {
        if img.data[start] > threshold || label[start] != u32::MAX {
            continue;
        }
        let id = start as u32;
        let mut c = Component { min_x: usize::MAX, min_y: usize::MAX, ..Default::default() };
        label[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            c.area += 1;
            c.sum_x += x as u64;
            c.sum_y += y as u64;
            c.min_x = c.min_x.min(x);
            c.max_x = c.max_x.max(x);
            c.min_y = c.min_y.min(y);
            c.max_y = c.max_y.max(y);
            if x == 0 || y == 0 || x == w - 1 || y == h - 1 {
                c.touches_border = true;
            }
            let mut visit = |j: usize| {
                if img.data[j] <= threshold && label[j] == u32::MAX {
                    label[j] = id;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }

        if c.touches_border {
            continue;
        }
        let bw = (c.max_x - c.min_x + 1) as f64;
        let bh = (c.max_y - c.min_y + 1) as f64;
        let radius = (bw + bh) / 4.0;
        if radius < cfg.min_radius_px || (bw / bh - 1.0).abs() > cfg.max_aspect_deviation {
            continue;
        }
        let fill = c.area as f64 / (std::f64::consts::PI * radius * radius);
        if fill < cfg.min_fill || fill > 1.15 {
            continue;
        }
        if best.as_ref().is_none_or(|(r, _)| radius > *r) {
            best = Some((radius, c));
        }
    }

    best.map(|(radius, c)| MarkDetection {
        // +0.5: pixel centres, so a mark centred in the image maps to w/2.
        center: [
            c.sum_x as f64 / c.area as f64 + 0.5,
            c.sum_y as f64 / c.area as f64 + 0.5,
        ],
        radius,
        threshold,
    })
}

/// Aim point relative to the mark centre in units of the mark's radius
/// (y up), assuming the aim point is the image centre. Independent of the
/// target type; multiply by [`TargetSpec::black_radius`] for millimetres.
pub fn aim_point_radii(det: &MarkDetection, width: usize, height: usize) -> [f64; 2] {
    let cx = width as f64 / 2.0;
    let cy = height as f64 / 2.0;
    [(cx - det.center[0]) / det.radius, (det.center[1] - cy) / det.radius]
}

/// Converts a detection into the aim point on the target (mm, y up).
pub fn aim_point_mm(det: &MarkDetection, width: usize, height: usize, spec: &TargetSpec) -> [f64; 2] {
    let r = aim_point_radii(det, width, height);
    [r[0] * spec.black_radius, r[1] * spec.black_radius]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Renders a synthetic air rifle target: white paper, black disc with
    /// white ring lines, centred at `(cx, cy)` with black radius `r` px.
    pub(crate) fn render_target(w: usize, h: usize, cx: f64, cy: f64, r: f64) -> Vec<u8> {
        let spec = TargetSpec::AIR_RIFLE_10M;
        let px_per_mm = r / spec.black_radius;
        let line = 0.7; // ring line half-width in px
        let mut img = vec![235u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let on_line = (5..=9).any(|n| (d - spec.ring_radius(n) * px_per_mm).abs() < line);
                if d <= r && !on_line {
                    img[y * w + x] = 20;
                }
            }
        }
        img
    }

    #[test]
    fn finds_centred_mark() {
        let (w, h) = (320, 240);
        let data = render_target(w, h, 160.0, 120.0, 60.0);
        let det = find_mark(&GrayImage { width: w, height: h, data: &data }, &Default::default()).unwrap();
        assert!((det.center[0] - 160.0).abs() < 0.3, "{det:?}");
        assert!((det.center[1] - 120.0).abs() < 0.3, "{det:?}");
        assert!((det.radius - 60.0).abs() < 1.5, "{det:?}");
        let aim = aim_point_mm(&det, w, h, &TargetSpec::AIR_RIFLE_10M);
        assert!(aim[0].abs() < 0.1 && aim[1].abs() < 0.1, "{aim:?}");
    }

    #[test]
    fn offset_mark_gives_aim_offset() {
        // Mark appears left of and below the image centre, so the barrel
        // points right of and above the target centre.
        let (w, h) = (320, 240);
        let data = render_target(w, h, 130.0, 150.0, 60.0);
        let det = find_mark(&GrayImage { width: w, height: h, data: &data }, &Default::default()).unwrap();
        let aim = aim_point_mm(&det, w, h, &TargetSpec::AIR_RIFLE_10M);
        let mm_per_px = 15.25 / 60.0;
        assert!((aim[0] - 30.0 * mm_per_px).abs() < 0.3, "{aim:?}");
        assert!((aim[1] - 30.0 * mm_per_px).abs() < 0.3, "{aim:?}");
    }

    #[test]
    fn ignores_dark_border_and_blank_frames() {
        let (w, h) = (100, 80);
        let blank = vec![200u8; w * h];
        assert!(find_mark(&GrayImage { width: w, height: h, data: &blank }, &Default::default()).is_none());

        // A dark band touching the image edge is not a mark.
        let mut band = vec![220u8; w * h];
        for y in 0..20 {
            for x in 0..w {
                band[y * w + x] = 10;
            }
        }
        assert!(find_mark(&GrayImage { width: w, height: h, data: &band }, &Default::default()).is_none());
    }
}
