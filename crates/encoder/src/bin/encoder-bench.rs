//! encoder-bench - measures encoders on BGRA frames and verifies the output decodes.
//! Synthetic content (default) = desktop-like: static background + a moving window with scrolling text rows.

use std::io::Write;
use std::time::Instant;

use clap::Parser;
use encoder::{AdaptiveEncoder, EncoderKind, EncoderSettings, Frame, PixelFormat};

#[derive(Parser, Debug)]
#[command(name = "encoder-bench", version, about = "Measure FastRDP encoders and verify the output decodes")]
struct Cli {
    /// Encoder to request (falls back to OpenH264 like the server): openh264, nvenc, mf
    #[arg(long)]
    encoder: Option<EncoderKind>,
    /// encoder.toml to load (default: built-in defaults)
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// OpenH264 complexity override: low, medium, high
    #[arg(long)]
    complexity: Option<String>,
    /// OpenH264 threads override (0 = automatic, 1 = single)
    #[arg(long)]
    threads: Option<u16>,
    /// NVENC preset override: p1 .. p7
    #[arg(long)]
    preset: Option<String>,
    /// Frame rate the encoder is configured for (the session's refresh rate)
    #[arg(long)]
    fps: Option<u32>,
    /// Rate at which frames actually arrive (the remote app's frame rate; default: --fps)
    #[arg(long)]
    source_fps: Option<u32>,
    /// Synthetic content: the whole screen is text scrolling up (a console) instead of a moving window
    #[arg(long)]
    scroll: bool,
    /// Target bitrate (kbps)
    #[arg(long)]
    bitrate_kbps: Option<u32>,
    /// Number of frames to encode
    #[arg(long, default_value_t = 600)]
    frames: usize,
    /// Synthetic frame width
    #[arg(long, default_value_t = 1920)]
    width: u32,
    /// Synthetic frame height
    #[arg(long, default_value_t = 1080)]
    height: u32,
    /// Use this BMP as the base frame instead of synthetic content
    #[arg(long)]
    input: Option<String>,
    /// Write the H.264 elementary stream here
    #[arg(long)]
    out: Option<std::path::PathBuf>,
    /// Simulate an encoder failure at frame N (tests the fallback path)
    #[arg(long, value_name = "N")]
    inject_failure_at: Option<usize>,
}

fn load_bmp(path: &str) -> (u32, u32, Vec<u8>) {
    let b = std::fs::read(path).expect("read bmp");
    let off = u32::from_le_bytes(b[10..14].try_into().unwrap()) as usize;
    let w = i32::from_le_bytes(b[18..22].try_into().unwrap());
    let h = i32::from_le_bytes(b[22..26].try_into().unwrap());
    let bpp = u16::from_le_bytes(b[28..30].try_into().unwrap());
    assert_eq!(bpp, 32, "expects a 32-bit BMP (FastRdpIdd frame dump)");
    let (w, hh) = (w as usize, h.unsigned_abs() as usize);
    let mut out = vec![0u8; w * hh * 4];
    for y in 0..hh {
        let sy = if h < 0 { y } else { hh - 1 - y };
        out[y * w * 4..(y + 1) * w * 4].copy_from_slice(&b[off + sy * w * 4..off + (sy + 1) * w * 4]);
    }
    (w as u32, hh as u32, out)
}

/// Full-screen text rows scrolling up by one row per frame, like a console printing output.
fn synth_scroll(w: usize, h: usize, t: usize, out: &mut [u8]) {
    const ROW: usize = 16;
    for y in 0..h {
        let line = y / ROW + t; // the text moves up one row per frame
        let glyph_row = y % ROW;
        for x in 0..w {
            let col = x / 8;
            // pseudo-random "characters" per (line, column), 8x16 cells with a 2-pixel gap
            let seed = (line as u32).wrapping_mul(2_654_435_761) ^ (col as u32).wrapping_mul(40_503);
            let len = (line * 37 % 90 + 10) * w / 100 / 8;
            let ink = col < len && glyph_row < 13 && (x % 8) < 7 && (seed >> ((glyph_row + x % 8) % 24)) & 1 == 1;
            let i = (y * w + x) * 4;
            let v = if ink { 204 } else { 12 };
            out[i..i + 4].copy_from_slice(&[v, v, v, 255]);
        }
    }
}

fn synth(base: &[u8], w: usize, h: usize, t: usize, out: &mut [u8]) {
    out.copy_from_slice(base);
    let (ww, wh) = (400.min(w / 2), 300.min(h / 2));
    let (wx, wy) = ((t * 7) % (w - ww), (t * 3) % (h - wh));
    for y in wy..wy + wh {
        for x in wx..wx + ww {
            let i = (y * w + x) * 4;
            out[i] = 240;
            out[i + 1] = (200 + (y - wy) / 8).min(255) as u8;
            out[i + 2] = 60;
        }
    }
    for row in 0..(wh / 25) {
        let y = wy + 10 + row * 22;
        for x in wx + 10..wx + ww - 10 {
            if !((x + t * 2 + row * 13) / 5).is_multiple_of(3) {
                for dy in 0..10 {
                    let i = ((y + dy) * w + x) * 4;
                    out[i] = 20;
                    out[i + 1] = 20;
                    out[i + 2] = 20;
                }
            }
        }
    }
}

fn psnr_y(src_bgra: &[u8], w: usize, h: usize, dec: &openh264::decoder::DecodedYUV<'_>) -> f64 {
    use openh264::formats::YUVSource;
    let mut r = encoder::convert::I420::new(w, h);
    encoder::convert::bgra_to_i420(src_bgra, w * 4, &mut r, 0);
    let (ys, _, _) = dec.strides();
    let dy = dec.y();
    let mut se = 0f64;
    for y in 0..h {
        for x in 0..w {
            let d = r.y[y * w + x] as f64 - dy[y * ys + x] as f64;
            se += d * d;
        }
    }
    let mse = se / (w * h) as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

fn main() {
    let cli = Cli::parse();
    let mut s = match &cli.config {
        Some(p) => EncoderSettings::load(p).expect("config"),
        None => EncoderSettings::default(),
    };
    if let Some(e) = cli.encoder {
        s.encoder = e;
    }
    if let Some(c) = cli.complexity.clone() {
        s.openh264.complexity = c;
    }
    if let Some(t) = cli.threads {
        s.openh264.threads = t;
    }
    if let Some(p) = cli.preset.clone() {
        s.nvenc.preset = p;
    }
    if let Some(f) = cli.fps {
        s.fps = f;
    }
    if let Some(b) = cli.bitrate_kbps {
        s.bitrate_kbps = b;
    }
    let frames = cli.frames;
    let inject_at = cli.inject_failure_at;

    let (w, h, base) = match &cli.input {
        Some(p) => load_bmp(p),
        None => {
            let (w, h) = (cli.width, cli.height);
            let mut b = vec![0u8; (w * h * 4) as usize];
            for (i, px) in b.chunks_mut(4).enumerate() {
                let (x, y) = (i % w as usize, i / w as usize);
                px.copy_from_slice(&[(100 + y / 12).min(255) as u8, (60 + x / 20).min(255) as u8, 40, 255]);
            }
            (w, h, b)
        }
    };
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    println!("frames={frames} size={w}x{h} requested={} cpus={cpus}", s.encoder);
    // the CPU colour conversion alone (part of every software-encoder frame below)
    let mut yuv = encoder::convert::I420::new(w as usize, h as usize);
    let t = Instant::now();
    for _ in 0..100 {
        encoder::convert::bgra_to_i420(&base, w as usize * 4, &mut yuv, 0);
    }
    println!("BGRA->I420 conversion alone: {:.2} ms", t.elapsed().as_secs_f64() * 1e3 / 100.0);

    let t0 = Instant::now();
    let mut enc = AdaptiveEncoder::new(s.clone(), w, h).expect("encoder (OpenH264 fallback also failed)");
    println!("init {:.1} ms active={} ({})", t0.elapsed().as_secs_f64() * 1e3, enc.active(), enc.describe());
    for ev in enc.take_events() {
        println!("FALLBACK {:?}: {} -> {} : {}", ev.phase, ev.from, ev.to, ev.reason);
    }

    let mut out_file = cli.out.as_ref().map(|p| std::fs::File::create(p).expect("out"));
    let mut dec = openh264::decoder::Decoder::new().expect("decoder");
    let mut buf = vec![0u8; base.len()];
    let mut times = Vec::with_capacity(frames);
    let (mut bytes, mut keys, mut decoded, mut psnr_sum, mut psnr_n) = (0usize, 0usize, 0usize, 0f64, 0usize);
    let mut by_encoder = std::collections::BTreeMap::<String, usize>::new();
    let wall = Instant::now();
    let source_fps = cli.source_fps.unwrap_or(s.fps).max(1);
    for i in 0..frames {
        if cli.scroll {
            synth_scroll(w as usize, h as usize, i, &mut buf);
        } else {
            synth(&base, w as usize, h as usize, i, &mut buf);
        }
        if inject_at == Some(i) {
            enc.inject_runtime_failures(1);
        }
        let f =
            Frame { width: w, height: h, stride: w as usize * 4, format: PixelFormat::Bgra, data: &buf, pts_us: (i as i64) * 1_000_000 / source_fps as i64 };
        let t = Instant::now();
        let p = enc.encode(&f).expect("encode");
        times.push(t.elapsed().as_secs_f64() * 1e3);
        for ev in enc.take_events() {
            println!("FALLBACK at frame {i} {:?}: {} -> {} : {}", ev.phase, ev.from, ev.to, ev.reason);
        }
        let Some(p) = p else { continue };
        bytes += p.data.len();
        keys += p.keyframe as usize;
        *by_encoder.entry(p.encoder.to_string()).or_default() += 1;
        if let Some(f) = out_file.as_mut() {
            f.write_all(&p.data).unwrap();
        }
        match dec.decode(&p.data) {
            Ok(Some(y)) => {
                decoded += 1;
                if i % 10 == 0 {
                    psnr_sum += psnr_y(&buf, w as usize, h as usize, &y);
                    psnr_n += 1;
                }
            }
            Ok(None) => {}
            Err(e) => println!("DECODE ERROR at frame {i}: {e}"),
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted[((sorted.len() as f64 - 1.0) * p) as usize];
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    println!(
        "convert+encode ms: avg {avg:.2} p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} -> {:.1} fps capacity",
        pct(0.5),
        pct(0.95),
        pct(0.99),
        pct(1.0),
        1000.0 / avg
    );
    let mbps = bytes as f64 * 8.0 / (frames as f64 / source_fps as f64) / 1e6;
    let psnr = if psnr_n > 0 { psnr_sum / psnr_n as f64 } else { 0.0 };
    println!(
        "bitrate {mbps:.1} Mbps ({:.0} kbit/frame) @{source_fps}fps, encoder at {}fps | keyframes {keys} | decoded {decoded}/{frames} (OpenH264 decoder) | PSNR-Y {psnr:.2} dB",
        bytes as f64 * 8.0 / 1000.0 / frames as f64,
        s.fps
    );
    println!("packets by encoder {by_encoder:?} | wall {wall_s:.2}s | final active={} fallback={}", enc.active(), enc.is_fallback());
}
