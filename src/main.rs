use anyhow::{bail, Context as _, Result};
use clap::Parser;
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};

use symphonia::core::audio::sample::Sample;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::default::*;

// ── Defaults (overridable via CLI) ────────────────────────────────────
const DEFAULT_FPS: u32 = 30;
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_BAR_WIDTH: u32 = 4;
const DEFAULT_WINDOW_SECS: f64 = 2.0;

// ── Audio data ────────────────────────────────────────────────────────
struct AudioData {
    /// Mono PCM samples normalised to f32 in [-1.0, 1.0].
    samples: Vec<f32>,
    /// Sample rate in Hz.
    sample_rate: u32,
}

// ── CLI ───────────────────────────────────────────────────────────────
#[derive(Parser, Debug)]
#[command(version, about = "Generate a waveform video from an audio file")]
struct Args {
    /// Input audio file
    #[arg(short, long)]
    input: String,

    /// Output video file (e.g. output.mp4)
    #[arg(short, long, default_value = "output.mp4")]
    output: String,

    /// Frames per second (any value works, e.g. 30, 60, 128, 256)
    #[arg(short = 'r', long, default_value_t = DEFAULT_FPS)]
    fps: u32,

    /// Video width in pixels
    #[arg(long, default_value_t = DEFAULT_WIDTH)]
    width: u32,

    /// Video height in pixels
    #[arg(long, default_value_t = DEFAULT_HEIGHT)]
    height: u32,

    /// Width of each amplitude bar in pixels
    #[arg(long, default_value_t = DEFAULT_BAR_WIDTH)]
    bar_width: u32,

    /// Seconds of audio visible on screen at once
    #[arg(long, default_value_t = DEFAULT_WINDOW_SECS)]
    window: f64,
}

// ── Entry point ───────────────────────────────────────────────────────
fn main() -> Result<()> {
    let args = Args::parse();

    let audio = decode_audio(&args.input)?;
    generate_video(&audio, &args)?;

    Ok(())
}

// ── Decode audio to mono f32 PCM ─────────────────────────────────────
fn decode_audio(path: &str) -> Result<AudioData> {
    let file = File::open(path).with_context(|| format!("failed to open audio file: {path}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let hint = Hint::new();
    let mut format = get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;

    let track = format.default_track(TrackType::Audio).unwrap();
    let track_id = track.id;

    let dec_opts = AudioDecoderOptions::default();
    let mut decoder = get_codecs()
        .make_audio_decoder(track.codec_params.as_ref().unwrap().audio().unwrap(), &dec_opts)
        .unwrap();

    // Read per-channel interleaved samples.
    let mut buf = Vec::<f32>::new();
    let mut sample_rate = 0u32;
    let mut num_ch = 1usize;

    while let Some(packet) = format.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                if sample_rate == 0 {
                    sample_rate = audio_buf.spec().rate();
                    num_ch = audio_buf.spec().channels().count();
                }
                let n = audio_buf.samples_interleaved();
                let start = buf.len();
                buf.resize(start + n, f32::MID);
                audio_buf.copy_to_slice_interleaved(&mut buf[start..]);
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    // Down-mix to mono.
    let frames = buf.len() / num_ch;
    let mut mono = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut sum = 0.0f32;
        for ch in 0..num_ch {
            sum += buf[f * num_ch + ch];
        }
        mono.push(sum / num_ch as f32);
    }

    eprintln!(
        "Decoded {:.1}s audio ({} Hz, {} ch → mono, {} samples)",
        mono.len() as f64 / sample_rate as f64,
        sample_rate,
        num_ch,
        mono.len(),
    );

    Ok(AudioData { samples: mono, sample_rate })
}

// ── Video generation (pipe to ffmpeg CLI) ────────────────────────────
fn generate_video(audio: &AudioData, args: &Args) -> Result<()> {
    let fps = args.fps;
    let width = args.width;
    let height = args.height;
    let bar_width = args.bar_width;
    let window_secs = args.window;
    let output_path = &args.output;
    let audio_path = &args.input;

    let total_samples = audio.samples.len();
    let sample_rate = audio.sample_rate as f64;
    let total_dur = total_samples as f64 / sample_rate;
    let total_frames = (total_dur * fps as f64).ceil() as u32;
    let num_bars = (width / bar_width) as usize;

    let frame_size = (width * height * 3) as usize;
    let mut raw_frame: Vec<u8> = vec![0u8; frame_size];

    eprintln!(
        "Generating {total_frames} frames ({width}×{height} @ {fps} fps) ..."
    );

    // ── Launch ffmpeg as a child process, pipe raw frames to stdin ──
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "rawvideo",
            "-pixel_format", "rgb24",
            "-video_size", &format!("{width}x{height}"),
            "-framerate", &fps.to_string(),
            "-i", "-",             // read raw video from stdin
            "-i", audio_path,      // audio input
            "-c:v", "libx264",
            "-preset", "medium",
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "192k",
            "-shortest",
            "-movflags", "+faststart",
            "-vf", "format=yuv420p",
            output_path,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit()) // show ffmpeg progress
        .spawn()
        .with_context(|| "failed to spawn ffmpeg – is it installed?")?;

    let mut stdin = ffmpeg.stdin.take().expect("failed to open ffmpeg stdin");

    let mut last_pct = 0u32;

    for frame in 0..total_frames {
        let pct = frame * 100 / total_frames;
        if pct >= last_pct + 5 {
            last_pct = pct;
            eprintln!("  {pct}%  (frame {frame}/{total_frames})");
        }

        render_raw_rgb24(audio, frame, fps, width, height, bar_width, window_secs, num_bars, &mut raw_frame);

        // Write raw RGB24 bytes to ffmpeg's stdin.
        stdin.write_all(&raw_frame)?;
    }

    // Close stdin so ffmpeg knows the video stream is done.
    drop(stdin);

    let status = ffmpeg.wait()?;
    if !status.success() {
        bail!("ffmpeg encoding failed (exit status: {status})");
    }

    eprintln!("✓ Video saved to {output_path}");
    Ok(())
}

// ── Render one frame into a raw RGB24 buffer ────────────────────────
fn render_raw_rgb24(
    audio: &AudioData,
    frame_idx: u32,
    fps: u32,
    width: u32,
    height: u32,
    bar_width: u32,
    window_secs: f64,
    num_bars: usize,
    buf: &mut [u8],
) {
    let sample_rate = audio.sample_rate as f64;
    let total_samples = audio.samples.len();
    let cx = (height / 2) as i32;

    let t = frame_idx as f64 / fps as f64;

    let window_start_t = (t - window_secs).max(0.0);
    let win_start = (window_start_t * sample_rate) as usize;
    let win_end = (t * sample_rate) as usize;
    let win_len = win_end.saturating_sub(win_start);

    let stride = (width * 3) as usize;

    // ── Fill background ──────────────────────────────────────────
    for pixel in buf.chunks_exact_mut(3) {
        pixel.copy_from_slice(&[8, 8, 24]);
    }

    // ── Centre line ──────────────────────────────────────────────
    let center_y = cx as u32;
    if center_y < height {
        let row_start = center_y as usize * stride;
        for x in 0..width as usize {
            let p = row_start + x * 3;
            buf[p..p + 3].copy_from_slice(&[30, 30, 50]);
        }
    }

    // ── Draw bars ────────────────────────────────────────────────
    if win_len > 0 {
        let samples_per_bar = (win_len as f64 / num_bars as f64).ceil() as usize;

        for bar in 0..num_bars {
            let s_start = win_start + bar * samples_per_bar;
            let s_end = (win_start + (bar + 1) * samples_per_bar)
                .min(win_end)
                .min(total_samples);

            if s_start >= total_samples || s_start >= s_end {
                continue;
            }

            let max_abs = audio.samples[s_start..s_end]
                .iter()
                .map(|s| s.abs())
                .fold(0.0f32, f32::max);

            let half_h = (height as f32 / 2.0 * max_abs) as u32;
            let half_h = half_h.min(height / 2);

            let color = amplitude_color(max_abs);

            let x0 = bar as u32 * bar_width;
            let x1 = (x0 + bar_width).min(width);

            for x in x0..x1 {
                let x_base = x as usize * 3;
                for dy in 0..half_h {
                    let y_top = cx as u32 - half_h + dy;
                    if y_top < height {
                        let p = y_top as usize * stride + x_base;
                        buf[p..p + 3].copy_from_slice(&color);
                    }
                    let y_bot = cx as u32 + dy;
                    if y_bot < height {
                        let p = y_bot as usize * stride + x_base;
                        buf[p..p + 3].copy_from_slice(&color);
                    }
                }
            }
        }
    }

    // ── Playhead line at right edge ──────────────────────────────
    let px = width - 1;
    let x_base = px as usize * 3;
    for y in 0..height as usize {
        let p = y * stride + x_base;
        buf[p..p + 3].copy_from_slice(&[255, 255, 255]);
    }
}

/// Map amplitude [0, 1] to a colour: dark-blue → cyan → yellow.
fn amplitude_color(amp: f32) -> [u8; 3] {
    let t = amp.clamp(0.0, 1.0);
    let (r, g, b) = if t < 0.5 {
        let s = t / 0.5;
        (0u8, lerp(20.0, 200.0, s), lerp(80.0, 255.0, s))
    } else {
        let s = (t - 0.5) / 0.5;
        (lerp(0.0, 255.0, s), lerp(200.0, 255.0, s), lerp(255.0, 50.0, s))
    };
    [r, g, b]
}

fn lerp(a: f32, b: f32, t: f32) -> u8 {
    (a + (b - a) * t.clamp(0.0, 1.0)) as u8
}
