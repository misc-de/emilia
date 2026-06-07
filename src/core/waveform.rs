//! Waveform decoding and destructive region-cut for the recording editor.
//!
//! Recordings are short per-song captures, so the cut decodes the whole file to
//! interleaved S16 PCM in memory, drops the marked ranges, and re-encodes to the
//! same codec (MP3 / FLAC / Ogg-Opus). AAC has no reliably working encoder/muxer
//! across GStreamer installs, so an AAC source is rewritten as MP3 — its
//! extension (and thus path) then changes. The caller updates the DB row.

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use std::path::{Path, PathBuf};

/// Outcome of a cut: the (possibly changed) file path and the new duration (ms).
pub struct CutResult {
    pub path: PathBuf,
    pub duration_ms: i64,
}

/// Decodes `path` into `buckets` mono peak amplitudes (0.0–1.0) for the waveform
/// display, plus the total duration in seconds. Synchronous — run off the UI thread.
pub fn decode_peaks(path: &Path, buckets: usize) -> Result<(Vec<f32>, f64)> {
    let _ = gst::init();
    let buckets = buckets.max(1);
    let (samples, rate, _ch) = decode_pcm(path, true)?;
    if samples.is_empty() || rate == 0 {
        bail!("no audio decoded");
    }
    let duration = samples.len() as f64 / f64::from(rate);
    let mut peaks = vec![0.0f32; buckets];
    let n = samples.len();
    for (i, s) in samples.iter().enumerate() {
        let b = (i * buckets / n).min(buckets - 1);
        let a = f32::from(s.unsigned_abs()) / f32::from(i16::MAX);
        if a > peaks[b] {
            peaks[b] = a;
        }
    }
    Ok((peaks, duration))
}

/// Analysis frame and silence thresholds for [`snap_to_silence`].
const SNAP_HOP_SECS: f64 = 0.02;
/// A hop counts as "silence" when it sits clearly below the window's median
/// loudness — a real dip, not just a soft passage.
const SNAP_SILENCE_FACTOR: f32 = 0.35;
/// …or below this absolute RMS (≈ −34 dBFS), whichever is larger, so a loud
/// crossfade (no true gap) never triggers a false snap.
const SNAP_ABS_FLOOR: f32 = 0.02;

/// Decodes `path` (a short window cut from a stream buffer) to mono PCM and
/// returns the time — in seconds from the start of `path` — of the silence gap
/// closest to `target_secs`. Used to snap a raw ICY cut point onto the actual
/// quiet point between songs. Returns `None` when the window has no genuine
/// silence dip (e.g. a crossfade), so the caller keeps the raw ICY offset.
/// Synchronous — run off the UI thread.
pub fn snap_to_silence(path: &Path, target_secs: f64) -> Option<f64> {
    let _ = gst::init();
    let (samples, rate, _ch) = decode_pcm(path, true).ok()?;
    if samples.is_empty() || rate == 0 {
        return None;
    }
    let hop = ((f64::from(rate) * SNAP_HOP_SECS) as usize).max(1);
    // Short-term RMS per hop, normalised to 0.0–1.0.
    let rms: Vec<f32> = samples
        .chunks(hop)
        .map(|frame| {
            let sum: f64 = frame
                .iter()
                .map(|&s| {
                    let v = f64::from(s);
                    v * v
                })
                .sum();
            ((sum / frame.len() as f64).sqrt() as f32) / f32::from(i16::MAX)
        })
        .collect();
    if rms.len() < 3 {
        return None;
    }
    // Threshold from the window's own loudness: loud material (crossfade) keeps a
    // high bar that a mere soft passage can't clear, while a quiet window still
    // has the absolute floor to catch its gaps.
    let mut sorted = rms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let threshold = (median * SNAP_SILENCE_FACTOR).max(SNAP_ABS_FLOOR);
    // Among the silent hops, the one nearest the raw ICY marker: trust the
    // metadata's rough location and snap only to the local gap.
    let target_hop = (target_secs / SNAP_HOP_SECS).max(0.0) as usize;
    let best = rms
        .iter()
        .enumerate()
        .filter(|(_, &r)| r < threshold)
        .min_by_key(|(i, _)| i.abs_diff(target_hop))?;
    Some(best.0 as f64 * SNAP_HOP_SECS + SNAP_HOP_SECS / 2.0)
}

/// Removes the given time ranges (seconds) from the recording, re-encoding to the
/// same codec (AAC → MP3), and writes the result. Returns the final path and the
/// new duration. Tags are re-applied best-effort. The caller overwrites the row.
pub fn cut(
    path: &Path,
    cut_ranges: &[(f64, f64)],
    artist: Option<&str>,
    title: &str,
    album: Option<&str>,
) -> Result<CutResult> {
    let _ = gst::init();
    if cut_ranges.is_empty() {
        bail!("no cut ranges");
    }

    let (samples, rate, channels) = decode_pcm(path, false)?;
    if samples.is_empty() || rate == 0 || channels == 0 {
        bail!("no audio decoded");
    }
    let ch = channels as usize;
    let frames = samples.len() / ch;
    let to_frame = |t: f64| ((t.max(0.0) * f64::from(rate)).round() as usize).min(frames);

    // Marked ranges → frame ranges, sorted + merged.
    let mut cuts: Vec<(usize, usize)> = cut_ranges
        .iter()
        .map(|&(a, b)| {
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            (to_frame(a), to_frame(b))
        })
        .filter(|&(a, b)| b > a)
        .collect();
    cuts.sort_by_key(|r| r.0);

    // Keep = complement of the merged cuts within [0, frames].
    let mut kept: Vec<i16> = Vec::with_capacity(samples.len());
    let mut cursor = 0usize;
    for (a, b) in merge_ranges(&cuts) {
        if a > cursor {
            kept.extend_from_slice(&samples[cursor * ch..a * ch]);
        }
        cursor = cursor.max(b);
    }
    if cursor < frames {
        kept.extend_from_slice(&samples[cursor * ch..frames * ch]);
    }
    if kept.is_empty() {
        bail!("the cut would remove the whole recording");
    }

    let kept_frames = kept.len() / ch;
    let duration_ms = (kept_frames as f64 / f64::from(rate) * 1000.0).round() as i64;

    // Encoder by source container; AAC has no working encoder here → MP3.
    let src_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp3")
        .to_ascii_lowercase();
    let (out_ext, enc): (&str, &str) = match src_ext.as_str() {
        "flac" => ("flac", "flacenc"),
        "ogg" | "oga" | "opus" => ("ogg", "opusenc ! oggmux"),
        // mp3 + aac/m4a/unknown all go to MP3 (lame is universally available).
        _ => ("mp3", "lamemp3enc"),
    };
    let out_path = path.with_extension(out_ext);
    let tmp = path.with_extension(format!("{out_ext}.emilia-cut"));

    encode_pcm(&kept, rate, channels, enc, &tmp)
        .with_context(|| format!("encoding the edited recording ({out_ext}) failed"))?;

    // Place the result. If the extension changed (AAC → MP3), drop the original.
    if out_path != path {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, &out_path)
        .or_else(|_| {
            std::fs::copy(&tmp, &out_path)
                .map(|_| ())
                .and_then(|_| std::fs::remove_file(&tmp))
        })
        .context("failed to write the edited recording")?;

    reembed_tags(&out_path, out_ext, artist, title, album);
    Ok(CutResult {
        path: out_path,
        duration_ms,
    })
}

/// Decodes `path` to interleaved S16 PCM. With `mono`, downmixes to one channel
/// (waveform); otherwise keeps the native channel count (cut). Returns
/// (samples, rate, channels).
fn decode_pcm(path: &Path, mono: bool) -> Result<(Vec<i16>, u32, u32)> {
    let caps = if mono {
        "audio/x-raw,format=S16LE,channels=1,layout=interleaved"
    } else {
        "audio/x-raw,format=S16LE,layout=interleaved"
    };
    let desc = format!(
        "filesrc name=src ! decodebin ! audioconvert ! audioresample ! \
         {caps} ! appsink name=sink sync=false"
    );
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    pipeline
        .by_name("src")
        .context("no filesrc")?
        .set_property("location", path.to_string_lossy().as_ref());
    let appsink = pipeline
        .by_name("sink")
        .context("no appsink")?
        .downcast::<AppSink>()
        .map_err(|_| anyhow!("sink is not an appsink"))?;

    pipeline
        .set_state(gst::State::Playing)
        .context("failed to start the decode pipeline")?;

    let mut samples: Vec<i16> = Vec::new();
    let mut rate = 0u32;
    let mut channels = 0u32;
    // `pull_sample` blocks until the next buffer and returns Err on EOS/error.
    while let Ok(sample) = appsink.pull_sample() {
        if rate == 0 {
            if let Some(s) = sample
                .caps()
                .and_then(|c| c.structure(0).map(|s| s.to_owned()))
            {
                rate = s.get::<i32>("rate").unwrap_or(0).max(0) as u32;
                channels = s.get::<i32>("channels").unwrap_or(0).max(0) as u32;
            }
        }
        if let Some(buf) = sample.buffer() {
            if let Ok(map) = buf.map_readable() {
                let data = map.as_slice();
                samples.reserve(data.len() / 2);
                for c in data.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([c[0], c[1]]));
                }
            }
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    Ok((samples, rate, channels))
}

/// Pushes interleaved S16 PCM through `appsrc ! audioconvert ! {enc} ! filesink`
/// and waits for EOS (or an encoder error) on the bus.
fn encode_pcm(samples: &[i16], rate: u32, channels: u32, enc: &str, out: &Path) -> Result<()> {
    let desc =
        format!("appsrc name=src ! audioconvert ! audioresample ! {enc} ! filesink name=sink");
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let appsrc = pipeline
        .by_name("src")
        .context("no appsrc")?
        .downcast::<AppSrc>()
        .map_err(|_| anyhow!("src is not an appsrc"))?;
    pipeline
        .by_name("sink")
        .context("no filesink")?
        .set_property("location", out.to_string_lossy().as_ref());

    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("layout", "interleaved")
        .field("rate", rate as i32)
        .field("channels", channels as i32)
        .build();
    appsrc.set_caps(Some(&caps));
    appsrc.set_property("format", gst::Format::Time);
    appsrc.set_is_live(false);
    appsrc.set_property("block", true);

    pipeline
        .set_state(gst::State::Playing)
        .context("failed to start the encode pipeline")?;

    // One buffer with the whole kept PCM (recordings are short). PTS/duration are
    // set so timestamp-sensitive encoders/muxers are happy.
    let frames = (samples.len() / channels.max(1) as usize) as u64;
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut buffer = gst::Buffer::from_mut_slice(bytes);
    {
        let b = buffer.get_mut().expect("fresh buffer is writable");
        b.set_pts(gst::ClockTime::ZERO);
        b.set_duration(gst::ClockTime::from_nseconds(
            frames * 1_000_000_000 / u64::from(rate.max(1)),
        ));
    }
    appsrc
        .push_buffer(buffer)
        .map_err(|e| anyhow!("pushing PCM failed: {e:?}"))?;
    appsrc
        .end_of_stream()
        .map_err(|e| anyhow!("signalling EOS failed: {e:?}"))?;

    let bus = pipeline.bus().context("no bus")?;
    let result = match bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(120),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(msg) => match msg.view() {
            gst::MessageView::Error(e) => Err(anyhow!("encoder error: {}", e.error())),
            _ => Ok(()),
        },
        None => Err(anyhow!("the encoder timed out")),
    };
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Merges a sorted list of `[start, end)` frame ranges.
fn merge_ranges(sorted: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &(a, b) in sorted {
        if let Some(last) = out.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        out.push((a, b));
    }
    out
}

/// Re-applies artist/title/album to the freshly encoded file (best effort). The
/// in-app cover survives via the cover cache (keyed by artist+title), so it is
/// not re-embedded here.
fn reembed_tags(path: &Path, ext: &str, artist: Option<&str>, title: &str, album: Option<&str>) {
    use lofty::config::WriteOptions;
    use lofty::prelude::{Accessor, TagExt};
    use lofty::tag::{Tag, TagType};

    let tag_type = match ext {
        "mp3" => TagType::Id3v2,
        "flac" | "ogg" | "oga" | "opus" => TagType::VorbisComments,
        _ => return,
    };
    let mut tag = Tag::new(tag_type);
    tag.set_title(title.to_string());
    if let Some(a) = artist.filter(|a| !a.trim().is_empty()) {
        tag.set_artist(a.to_string());
    }
    if let Some(al) = album.filter(|a| !a.trim().is_empty()) {
        tag.set_album(al.to_string());
    }
    if let Err(e) = tag.save_to_path(path, WriteOptions::default()) {
        tracing::debug!(
            "Could not re-tag the edited recording {}: {e}",
            path.display()
        );
    }
}
