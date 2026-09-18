//! raven-export: turn a screen recording into a video anything plays.
//!
//! ```sh
//! raven-export ~/Videos/Recordings/Recording-2026-09-13-101500.rvr
//! raven-export recording.rvr -o talk.mp4 --quality 18
//! ```
//!
//! Huginn records into Raven's own lossless format (`raven-rec`), which is
//! cheap to write while the desktop is in use and which nothing else opens.
//! This reads one, converts each frame to YUV, encodes it with Raven's own
//! H.264 encoder (`raven-h264`) and writes an MP4 with Raven's own writer
//! (`raven-mp4`). Nothing outside the tree is involved, so nothing outside
//! the tree can break it.
//!
//! The video keeps the recording's timing: frames are only recorded when the
//! screen changes, and each lasts until the next.

mod color;

use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use raven_h264::{Config, Encoder, Picture};

const USAGE: &str =
    "usage: raven-export RECORDING.rvr [-o OUTPUT.mp4] [--quality 0-51] [--keyframes SECONDS]

  -o, --output      where to write the video (default: the recording's name, .mp4)
  -q, --quality     the quantiser: lower is sharper and larger (default 22)
  -k, --keyframes   the most seconds between key frames, where players can seek (default 5)";

#[derive(Debug)]
struct Options {
    input: PathBuf,
    output: PathBuf,
    qp: u8,
    keyframes: Duration,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut input = None;
    let mut output = None;
    let mut qp = 22;
    let mut keyframes = Duration::from_secs(5);
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "-o" | "--output" => output = Some(PathBuf::from(value(&arg)?)),
            "-q" | "--quality" => {
                qp = value(&arg)?
                    .parse()
                    .ok()
                    .filter(|&q| q <= raven_h264::MAX_QP)
                    .ok_or("the quality is a number from 0 to 51")?;
            }
            "-k" | "--keyframes" => {
                let seconds: f64 = value(&arg)?
                    .parse()
                    .ok()
                    .filter(|&s: &f64| s > 0.0 && s.is_finite())
                    .ok_or("the key frame interval is a number of seconds")?;
                keyframes = Duration::from_secs_f64(seconds);
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            path if input.is_none() => input = Some(PathBuf::from(path)),
            extra => return Err(format!("one recording at a time (and {extra}?)")),
        }
    }
    let input = input.ok_or("which recording?")?;
    let output = output.unwrap_or_else(|| input.with_extension("mp4"));
    if output == input {
        return Err("the output would overwrite the recording".to_owned());
    }
    Ok(Options {
        input,
        output,
        qp,
        keyframes,
    })
}

/// Microseconds to track ticks, rounded.
fn ticks(time: Duration) -> u64 {
    let micros = u64::try_from(time.as_micros()).unwrap_or(u64::MAX / 90);
    (micros * 9 + 50) / 100
}

fn export(options: &Options) -> Result<(), Box<dyn Error>> {
    let file = File::open(&options.input)
        .map_err(|e| format!("opening {}: {e}", options.input.display()))?;
    let mut decoder = raven_rec::Decoder::new(BufReader::new(file))
        .map_err(|e| format!("reading {}: {e}", options.input.display()))?;
    let (src_w, src_h) = (decoder.width() as usize, decoder.height() as usize);
    let mut picture = color::I420::new(src_w, src_h);

    let config = Config {
        qp: options.qp,
        ..Config::new(picture.width as u32, picture.height as u32)
    };
    let mut encoder = Encoder::new(config)?;
    let video = raven_mp4::Video {
        width: picture.width as u32,
        height: picture.height as u32,
        sps: encoder.sps().to_vec(),
        pps: encoder.pps().to_vec(),
    };
    let mut mp4 = raven_mp4::Mp4::create(&options.output, Some(video), None)
        .map_err(|e| format!("creating {}: {e}", options.output.display()))?;

    let started = Instant::now();
    let mut last_report = started;
    let mut first = None;
    let mut last_key: Option<Duration> = None;
    let mut last_pts = Duration::ZERO;
    let mut frames = 0u64;
    let mut damaged = 0u64;
    loop {
        let frame = match decoder.next_frame() {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(raven_rec::Error::Truncated) => {
                eprintln!("\nthe recording was cut short; exporting what there is");
                break;
            }
            Err(raven_rec::Error::Io(e)) => return Err(e.into()),
            // A damaged record: carry on, and the picture comes back at the
            // recording's next key frame.
            Err(_) => {
                damaged += 1;
                continue;
            }
        };
        let first = *first.get_or_insert(frame.pts);
        let pts = frame.pts - first;
        picture.fill(frame.rgba, src_w, src_h);
        let key = last_key.is_none_or(|at| pts - at >= options.keyframes);
        let encoded = encoder.encode(
            &Picture {
                y: &picture.y,
                cb: &picture.cb,
                cr: &picture.cr,
            },
            key,
        )?;
        mp4.push_video(ticks(pts), &encoded.nals, encoded.idr)?;
        if encoded.idr {
            last_key = Some(pts);
        }
        last_pts = pts;
        frames += 1;
        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            eprint!("\r{frames} frames, {:.1} s of video", pts.as_secs_f64());
        }
    }
    if frames == 0 {
        let _ = std::fs::remove_file(&options.output);
        return Err("the recording has no frames".into());
    }

    // Without an end marker the last frame gets one frame's worth of time.
    let end = decoder
        .end()
        .and_then(|end| first.map(|first| end.saturating_sub(first)))
        .unwrap_or(last_pts + Duration::from_millis(33));
    let size = mp4.finish(ticks(end))?;
    let seconds = end.as_secs_f64().max(0.001);
    eprintln!(
        "\r{} — {frames} frames, {:.1} s, {:.1} MB ({:.0} kbit/s), in {:.1} s",
        options.output.display(),
        seconds,
        size as f64 / 1_000_000.0,
        size as f64 * 8.0 / 1000.0 / seconds,
        started.elapsed().as_secs_f64(),
    );
    if damaged > 0 {
        eprintln!("{damaged} damaged records were skipped");
    }
    Ok(())
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("raven-export: {message}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match export(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("raven-export: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Options, String> {
        parse(list.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn the_output_defaults_to_the_recording_as_mp4() {
        let options = args(&["a/Recording.rvr"]).unwrap();
        assert_eq!(options.output, PathBuf::from("a/Recording.mp4"));
        assert_eq!(options.qp, 22);
    }

    #[test]
    fn options_are_checked() {
        assert!(args(&[]).is_err());
        assert!(args(&["x.rvr", "--quality", "60"]).is_err());
        assert!(args(&["x.rvr", "--keyframes", "0"]).is_err());
        assert!(args(&["x.mp4", "-o", "x.mp4"]).is_err());
        let options = args(&["x.rvr", "-q", "18", "-k", "2.5", "-o", "y.mp4"]).unwrap();
        assert_eq!(options.qp, 18);
        assert_eq!(options.keyframes, Duration::from_millis(2500));
    }

    #[test]
    fn ticks_round_to_the_nearest() {
        assert_eq!(ticks(Duration::from_secs(1)), 90_000);
        assert_eq!(ticks(Duration::from_micros(33_333)), 3_000);
    }
}
