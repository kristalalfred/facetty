use std::time::Instant;

use bits_ascii::{Analyzer, Image, Params};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: render <image> [cols] [key=value...]");
    let cols: u16 = args.next().map(|s| s.parse().unwrap()).unwrap_or(96);
    let mut params = Params::default();
    let mut color = false;
    for kv in args {
        let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), ""));
        match k {
            "threshold" => params.threshold = v.parse().unwrap(),
            "edge" => params.edge_threshold = v.parse().unwrap(),
            "sigma" => params.sigma = v.parse().unwrap(),
            "tau" => params.tau = v.parse().unwrap(),
            "radius" => params.kernel_radius = v.parse().unwrap(),
            "exposure" => params.exposure = v.parse().unwrap(),
            "attenuation" => params.attenuation = v.parse().unwrap(),
            "noedges" => params.edges = false,
            "color" => color = true,
            _ => panic!("unknown option {k}"),
        }
    }
    let img = image::open(&path).unwrap().to_rgb8();
    let img = Image::new(img.width() as usize, img.height() as usize, img.into_raw());
    let rows = ((cols as u32 * 9 + 16) / 32) as u16;

    let mut analyzer = Analyzer::new(params);
    let t = Instant::now();
    analyzer.analyze(&img);
    let analyze = t.elapsed();
    let t = Instant::now();
    let frame = analyzer.render(cols, rows);
    let render = t.elapsed();
    let encoded = bits_ascii::encode(&frame);

    for (r, line) in frame.lines().enumerate() {
        if color {
            for (c, ch) in line.chars().enumerate() {
                let [red, g, b] = frame.get(c as u16, r as u16).rgb;
                print!("\x1b[38;2;{red};{g};{b}m{ch}");
            }
            println!("\x1b[0m");
        } else {
            println!("{line}");
        }
    }
    eprintln!(
        "{}x{} px -> {cols}x{rows} cells; analyze {analyze:?}, render {render:?}, encoded {} bytes",
        img.width,
        img.height,
        encoded.len()
    );
}
