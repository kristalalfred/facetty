//! Bakes frames rendered by `splash/phosphor.py` into a reel file.

use std::io::Read;
use std::path::{Path, PathBuf};

use facetty_ascii::reel::{self, Reel};
use facetty_ascii::{Analyzer, Frame, Geometry, Image, Params};

const ROWS: [u16; 8] = [22, 26, 30, 35, 40, 46, 53, 61];

struct Bloom {
    strength: f32,
    threshold: f32,
    radius: usize,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(config), Some(out)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: bake_splash <frames dir> <config> <out file>");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    let mut params = Params {
        edge_min_luma: 0.04,
        ..Params::default()
    };
    let mut bloom = Bloom {
        strength: 0.0,
        threshold: 0.7,
        radius: 24,
    };
    let (mut loop_frame, mut exit_frame) = (None, None);
    let config = std::fs::read_to_string(&config).expect("config");
    for line in config.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once('=').expect("key=value");
        let f = || v.parse::<f32>().unwrap();
        match k {
            "name" | "fps" => {}
            "loop" => loop_frame = Some(v.parse::<usize>().unwrap()),
            "exit" => exit_frame = Some(v.parse::<usize>().unwrap()),
            "radius" => params.kernel_radius = v.parse().unwrap(),
            "sigma" => params.sigma = f(),
            "tau" => params.tau = f(),
            "threshold" => params.threshold = f(),
            "edge" => params.edge_threshold = f(),
            "depth" => params.depth_threshold = f(),
            "normal" => params.normal_threshold = f(),
            "edge_min_luma" => params.edge_min_luma = f(),
            "exposure" => params.exposure = f(),
            "attenuation" => params.attenuation = f(),
            "bloom" => bloom.strength = f(),
            "bloom_threshold" => bloom.threshold = f(),
            "bloom_radius" => bloom.radius = v.parse().unwrap(),
            _ => panic!("unknown config key {k}"),
        }
    }

    let mut pngs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    pngs.sort();
    let index_of = |frame: Option<usize>| {
        let frame = frame.expect("config needs loop and exit");
        pngs.iter()
            .position(|p| number(p) == frame)
            .unwrap_or_else(|| panic!("no frame {frame}"))
    };
    let (loop_start, exit_start) = (index_of(loop_frame), index_of(exit_frame));
    let (w, h) = image::image_dimensions(&pngs[0]).unwrap();
    let sizes: Vec<(u16, u16)> = ROWS
        .iter()
        .map(|&r| ((r as f32 * 2.0 * w as f32 / h as f32).round() as u16, r))
        .collect();

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = pngs.len().div_ceil(threads);
    let per_frame: Vec<Vec<Frame>> = std::thread::scope(|s| {
        let handles: Vec<_> = pngs
            .chunks(chunk)
            .map(|paths| {
                let (sizes, bloom) = (&sizes, &bloom);
                s.spawn(move || {
                    let mut analyzer = Analyzer::new(params);
                    paths
                        .iter()
                        .map(|path| {
                            let (img, geometry) = load(path, bloom);
                            analyzer.analyze_geometry(&img, &geometry);
                            sizes.iter().map(|&(c, r)| analyzer.render(c, r)).collect()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });

    let reels: Vec<Reel> = (0..sizes.len())
        .map(|i| Reel {
            frames: per_frame.iter().map(|sizes| sizes[i].clone()).collect(),
            loop_start,
            exit_start,
        })
        .collect();
    let data = reel::encode(&reels);
    std::fs::write(&out, &data).unwrap();
    eprintln!(
        "{} frames at {} sizes from {}x{} px; {} KiB",
        pngs.len(),
        sizes.len(),
        w,
        h,
        data.len() / 1024
    );
}

fn number(path: &Path) -> usize {
    path.file_stem().unwrap().to_string_lossy().parse().unwrap()
}

/// Reads `NNNN.png` and the `NNNN.geo` that `splash/common.py` writes beside
/// it: u32 width, u32 height, f32 far, then zlib over u16 depth/far * 65535
/// and i8 normal * 127 per pixel.
fn load(png: &Path, bloom: &Bloom) -> (Image, Geometry) {
    let img = image::open(png).unwrap().to_rgb8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let rgb = glow(img.into_raw(), w, h, bloom);

    let geo = std::fs::read(png.with_extension("geo")).unwrap();
    let word = |i: usize| <[u8; 4]>::try_from(&geo[i * 4..i * 4 + 4]).unwrap();
    assert_eq!(
        (u32::from_le_bytes(word(0)), u32::from_le_bytes(word(1))),
        (w as u32, h as u32)
    );
    let far = f32::from_le_bytes(word(2));
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&geo[12..])
        .read_to_end(&mut raw)
        .unwrap();
    let (depth, normal) = raw.split_at(w * h * 2);
    let geometry = Geometry {
        width: w,
        height: h,
        depth: depth
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as f32 / 65535.0 * far)
            .collect(),
        normal: normal
            .chunks_exact(3)
            .map(|b| [b[0], b[1], b[2]].map(|v| v as i8 as f32 / 127.0))
            .collect(),
    };
    (Image::new(w, h, rgb), geometry)
}

fn glow(rgb: Vec<u8>, w: usize, h: usize, bloom: &Bloom) -> Vec<u8> {
    if bloom.strength <= 0.0 {
        return rgb;
    }
    let base: Vec<[f32; 3]> = rgb
        .chunks_exact(3)
        .map(|p| [p[0], p[1], p[2]].map(|v| v as f32 / 255.0))
        .collect();
    let mut bright: Vec<[f32; 3]> = base
        .iter()
        .map(|&c| {
            let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            let k = ((l - bloom.threshold) / (1.0 - bloom.threshold).max(1e-3)).clamp(0.0, 1.0);
            c.map(|v| v * k)
        })
        .collect();
    for _ in 0..3 {
        box_blur(&mut bright, w, h, bloom.radius / 2);
    }
    base.iter()
        .zip(&bright)
        .flat_map(|(a, b)| {
            std::array::from_fn::<u8, 3, _>(|k| {
                ((a[k] + b[k] * bloom.strength).min(1.0) * 255.0).round() as u8
            })
        })
        .collect()
}

fn box_blur(buf: &mut [[f32; 3]], w: usize, h: usize, r: usize) {
    if r == 0 {
        return;
    }
    let mut line = Vec::new();
    for y in 0..h {
        line.clear();
        line.extend_from_slice(&buf[y * w..(y + 1) * w]);
        blur_line(&line, r, |x, v| buf[y * w + x] = v);
    }
    for x in 0..w {
        line.clear();
        line.extend((0..h).map(|y| buf[y * w + x]));
        blur_line(&line, r, |y, v| buf[y * w + x] = v);
    }
}

fn blur_line(line: &[[f32; 3]], r: usize, mut set: impl FnMut(usize, [f32; 3])) {
    let n = line.len() as isize;
    let at = |i: isize| line[i.clamp(0, n - 1) as usize];
    let norm = 1.0 / (2 * r + 1) as f32;
    let r = r as isize;
    let mut acc = [0f32; 3];
    for i in -r..=r {
        let c = at(i);
        (0..3).for_each(|k| acc[k] += c[k]);
    }
    for i in 0..n {
        set(i as usize, acc.map(|a| a * norm));
        let (add, sub) = (at(i + r + 1), at(i - r));
        (0..3).for_each(|k| acc[k] += add[k] - sub[k]);
    }
}
