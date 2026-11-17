//! FM radio tuner: the MediaTek connsys FM chip of a phone (`/dev/fm`).
//!
//! The chip is driven with the ioctls of MediaTek's `fmradio` driver (magic
//! `0xf5`, the same interface Android's FM Radio app uses). Its sound does not
//! come through this device: the chip hands it to the phone's audio front end
//! over I2S, and the audio HAL routes it from there — on plinius the
//! `audio-hal-bridge` offers it as the PipeWire source [`SOURCE`]. Playback
//! therefore goes through [`crate::core::fm_src`], a GStreamer source for
//! `emilia-fm://<freq>` URIs that records that source and holds the tuner
//! switched on while it plays.
//!
//! The cable of wired headphones is the antenna. Without one the chip hears
//! noise only (measured: RSSI at −108 dBm, a band scan finds nothing).
//!
//! Frequencies are `u16` in units of 10 kHz, as the driver takes them
//! (`9860` = 98.6 MHz). The connac2x driver has no seek ioctl any more
//! (`FM_IOCTL_SEEK` answers `EPERM`): seeking and the band scan probe channel
//! by channel with `FM_IOCTL_SOFT_MUTE_TUNE`, which reports whether a channel
//! carries a station — what Android's FM app does on these chips too.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};

/// The tuner's character device.
pub const DEVICE: &str = "/dev/fm";
/// PipeWire/PulseAudio source that carries the tuner's sound (audio-hal-bridge).
pub const SOURCE: &str = "hal_fm_input";
/// URI scheme of FM playback: `emilia-fm://9860`.
pub const URI_SCHEME: &str = "emilia-fm";
/// The band (Europe/US): 87.5 – 108.0 MHz, in 10 kHz.
pub const FREQ_MIN: u16 = 8750;
pub const FREQ_MAX: u16 = 10800;
/// Channel raster: 100 kHz.
pub const STEP: u16 = 10;
/// Where a first start tunes to.
pub const FREQ_DEFAULT: u16 = 10000;

/// Node property by which an audio service may mark that it carries the
/// tuner's sound even while it keeps [`SOURCE`] out of the lists.
pub const SUPPORT_PROPERTY: &str = "audio-hal-bridge.fm";

/// Whether the device has a tuner that speaks this driver's interface: the
/// device opens for reading and writing and answers "is it powered up?"
/// (asking switches nothing on). Cheap, no audio involved.
pub fn tuner_present() -> bool {
    if !Path::new(DEVICE).exists() {
        return false;
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(DEVICE) else {
        return false;
    };
    let mut powered: u32 = 0;
    ioctl(&file, IOCTL_IS_POWERED_UP, &mut powered).is_ok()
}

/// What the platform offers for FM, once [`probe_support`] ran.
const SUPPORT_UNKNOWN: u8 = 0;
const SUPPORT_YES: u8 = 1;
const SUPPORT_NO: u8 = 2;
static SUPPORT: AtomicU8 = AtomicU8::new(SUPPORT_UNKNOWN);

/// Whether FM radio works on this platform: a tuner ([`tuner_present`]) *and*
/// an audio service that carries its sound ([`sound_present`]). `false` until
/// [`probe_support`] confirmed it.
pub fn supported() -> bool {
    SUPPORT.load(Ordering::SeqCst) == SUPPORT_YES
}

/// Finds out whether FM works here (see [`supported`]) and remembers it.
/// Blocking — the audio check asks the sound server — so run it off the GTK
/// thread.
pub fn probe_support() -> bool {
    let ok = tuner_present() && sound_present();
    SUPPORT.store(if ok { SUPPORT_YES } else { SUPPORT_NO }, Ordering::SeqCst);
    tracing::info!(
        "FM radio {}",
        if ok {
            "supported"
        } else {
            "not supported here"
        }
    );
    ok
}

/// Whether the sound server carries the tuner's sound: a node named
/// [`SOURCE`], or any node marked with [`SUPPORT_PROPERTY`]. Asked through
/// GStreamer's device monitor — over the PulseAudio protocol in the Flatpak,
/// the same way playback records the source.
fn sound_present() -> bool {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    if gst::init().is_err() {
        return false;
    }
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    monitor.add_filter(Some("Audio/Sink"), None);
    if monitor.start().is_err() {
        return false;
    }
    let found = monitor.devices().iter().any(|d| {
        let props = d.properties();
        let prop = |key: &str| {
            props
                .as_ref()
                .and_then(|p| p.get::<String>(key).ok())
                .unwrap_or_default()
        };
        // The pulse provider names the source in "internal-name", the
        // PipeWire provider in its properties.
        let internal = if d.has_property("internal-name") {
            d.property::<Option<String>>("internal-name")
                .unwrap_or_default()
        } else {
            String::new()
        };
        internal == SOURCE || prop("node.name") == SOURCE || prop(SUPPORT_PROPERTY) == "true"
    });
    monitor.stop();
    found
}

/// The playback URI of a frequency.
pub fn uri(freq: u16) -> String {
    format!("{URI_SCHEME}://{freq}")
}

/// The frequency of an `emilia-fm://` URI (snapped onto the band), if it is one.
pub fn parse_uri(uri: &str) -> Option<u16> {
    let rest = uri.strip_prefix(URI_SCHEME)?.strip_prefix("://")?;
    let digits = rest.trim_end_matches('/');
    let freq: u32 = digits.parse().ok()?;
    let freq = u16::try_from(freq).ok()?;
    (FREQ_MIN..=FREQ_MAX).contains(&freq).then(|| snap(freq))
}

/// Nearest channel of the raster inside the band.
pub fn snap(freq: u16) -> u16 {
    let f = freq.clamp(FREQ_MIN, FREQ_MAX);
    let off = (f - FREQ_MIN + STEP / 2) / STEP * STEP;
    (FREQ_MIN + off).min(FREQ_MAX)
}

/// The frequency in MHz with one decimal, without unit: "98.6".
pub fn mhz(freq: u16) -> String {
    format!("{}.{}", freq / 100, (freq % 100) / 10)
}

/// The frequency for display: "98.6 MHz".
pub fn label(freq: u16) -> String {
    format!("{} MHz", mhz(freq))
}

/// The channel `steps` channels away, wrapping around the band's ends.
pub fn step(freq: u16, steps: i32) -> u16 {
    let n = i32::from((FREQ_MAX - FREQ_MIN) / STEP) + 1;
    let i = i32::from((snap(freq) - FREQ_MIN) / STEP);
    FREQ_MIN + (i + steps).rem_euclid(n) as u16 * STEP
}

// ---- ioctls of MediaTek's fmradio driver (inc/fm_ioctl.h, inc/fm_main.h) ----

const FM_IOC_MAGIC: u64 = 0xf5;

/// `_IOWR(FM_IOC_MAGIC, nr, <pointer>)`: the driver declares every argument as
/// a pointer type, so the size field is the size of a pointer.
const fn iowr(nr: u64) -> u64 {
    (3 << 30) | ((std::mem::size_of::<usize>() as u64) << 16) | (FM_IOC_MAGIC << 8) | nr
}

const IOCTL_POWERUP: u64 = iowr(0);
const IOCTL_POWERDOWN: u64 = iowr(1);
const IOCTL_TUNE: u64 = iowr(2);
const IOCTL_IS_POWERED_UP: u64 = iowr(24);
const IOCTL_SOFT_MUTE_TUNE: u64 = iowr(63);

/// `struct fm_tune_parm` of the connac2x driver (with `deemphasis`; 8 bytes).
#[repr(C)]
#[derive(Default)]
struct TuneParm {
    err: u8,
    band: u8,
    space: u8,
    hilo: u8,
    deemphasis: u8,
    freq: u16,
}

/// `struct fm_softmute_tune_t`.
#[repr(C)]
#[derive(Default)]
struct SoftMuteTune {
    rssi: i32,
    freq: u16,
    valid: i32,
}

const BAND_UE: u8 = 1; // 87.5 – 108 MHz
const SPACE_100K: u8 = 1;
/// 0 = 50 µs (Europe); 1 would be 75 µs (Americas).
const DEEMPHASIS_50US: u8 = 0;

fn ioctl<T>(file: &File, request: u64, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is a live, exclusively borrowed `#[repr(C)]` value of the
    // layout the driver copies for this request.
    let r = unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The switched-on chip.
struct Tuner {
    file: File,
    freq: u16,
}

impl Tuner {
    fn power_up(freq: u16) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(DEVICE)?;
        let mut p = TuneParm {
            band: BAND_UE,
            space: SPACE_100K,
            deemphasis: DEEMPHASIS_50US,
            freq,
            ..Default::default()
        };
        ioctl(&file, IOCTL_POWERUP, &mut p)?;
        tracing::info!("FM tuner on at {}", label(freq));
        Ok(Self { file, freq })
    }

    fn tune(&mut self, freq: u16) -> io::Result<()> {
        let mut p = TuneParm {
            band: BAND_UE,
            space: SPACE_100K,
            deemphasis: DEEMPHASIS_50US,
            freq,
            ..Default::default()
        };
        ioctl(&self.file, IOCTL_TUNE, &mut p)?;
        self.freq = freq;
        Ok(())
    }

    /// Tunes to `freq` and reports whether a station is there (soft-mute tune).
    fn probe(&mut self, freq: u16) -> io::Result<bool> {
        let mut p = SoftMuteTune {
            freq,
            ..Default::default()
        };
        ioctl(&self.file, IOCTL_SOFT_MUTE_TUNE, &mut p)?;
        self.freq = freq;
        Ok(p.valid != 0)
    }
}

impl Drop for Tuner {
    fn drop(&mut self) {
        let mut rx: i32 = 0; // 0 = RX
        if let Err(e) = ioctl(&self.file, IOCTL_POWERDOWN, &mut rx) {
            tracing::warn!("FM power-down failed: {e}");
        } else {
            tracing::info!("FM tuner off");
        }
    }
}

/// The tuner is shared: playback holds it while it plays, a seek or band scan
/// while it runs. It is on while anyone holds a [`Lease`].
///
/// The lock is held through the chip's ioctls, which take their time (a TUNE
/// ~0.13 s, probing a channel with a station ~3 s, measured on plinius). The
/// GTK thread must therefore never wait for it: what it needs — the wanted
/// frequency and whether the tuner is on — lives in the atomics below.
struct Shared {
    tuner: Option<Tuner>,
    users: u32,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    tuner: None,
    users: 0,
});
/// The frequency playback wants (survives a scan, which tunes around).
static WANT: AtomicU16 = AtomicU16::new(FREQ_DEFAULT);
/// The tuner is switched on (written under the lock, read without it).
static POWERED: AtomicBool = AtomicBool::new(false);

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps the tuner switched on; the last one dropped switches it off.
pub struct Lease(());

impl Lease {
    /// Switches the tuner on (if it is not yet) tuned to `freq`, or retunes it.
    pub fn tuned(freq: u16) -> io::Result<Self> {
        let freq = snap(freq);
        WANT.store(freq, Ordering::SeqCst);
        let mut s = shared();
        match s.tuner.as_mut() {
            Some(t) => t.tune(freq)?,
            None => {
                s.tuner = Some(Tuner::power_up(freq)?);
                POWERED.store(true, Ordering::SeqCst);
            }
        }
        s.users += 1;
        Ok(Self(()))
    }

    /// Switches the tuner on where it is (for a seek or scan).
    fn any() -> io::Result<Self> {
        let mut s = shared();
        if s.tuner.is_none() {
            s.tuner = Some(Tuner::power_up(WANT.load(Ordering::SeqCst))?);
            POWERED.store(true, Ordering::SeqCst);
        }
        s.users += 1;
        Ok(Self(()))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = shared();
        s.users = s.users.saturating_sub(1);
        if s.users == 0 {
            POWERED.store(false, Ordering::SeqCst);
            s.tuner = None; // Tuner::drop powers down
        }
    }
}

/// Retunes a running tuner (blocking, for worker threads).
fn tune(freq: u16) -> io::Result<()> {
    let mut s = shared();
    match s.tuner.as_mut() {
        Some(t) => t.tune(freq),
        None => Ok(()),
    }
}

/// Retunes the running tuner without waiting for it — for the GTK thread (the
/// dial, next/previous). While a finger drags the dial, channels come faster
/// than the chip tunes, so a worker always tunes to the latest wish and skips
/// the ones in between. Without a running tuner the frequency is only
/// remembered for the next start.
pub fn request_tune(freq: u16) {
    static PENDING: AtomicU32 = AtomicU32::new(0);
    static WORKER: AtomicBool = AtomicBool::new(false);
    let freq = snap(freq);
    WANT.store(freq, Ordering::SeqCst);
    if !POWERED.load(Ordering::SeqCst) {
        return;
    }
    PENDING.store(u32::from(freq), Ordering::SeqCst);
    if WORKER.swap(true, Ordering::SeqCst) {
        return; // the running worker picks it up
    }
    std::thread::spawn(|| {
        loop {
            let f = PENDING.swap(0, Ordering::SeqCst);
            if f == 0 {
                WORKER.store(false, Ordering::SeqCst);
                // A wish that came in between the swap and the store.
                if PENDING.load(Ordering::SeqCst) != 0 && !WORKER.swap(true, Ordering::SeqCst) {
                    continue;
                }
                break;
            }
            // A seek or scan may hold the tuner meanwhile; it ends on WANT's
            // frequency or on the station it found.
            if let Err(e) = tune(f as u16) {
                tracing::warn!("FM tune to {} failed: {e}", label(f as u16));
            }
        }
    });
}

/// Seeks from `from` to the next station up or down, wrapping around the band
/// once. The tuner stays on the station found (or back on `from`). `None`:
/// nothing found, or `cancel` was set.
pub fn seek(from: u16, up: bool, cancel: &AtomicBool) -> io::Result<Option<u16>> {
    let _lease = Lease::any()?;
    let from = snap(from);
    let channels = i32::from((FREQ_MAX - FREQ_MIN) / STEP);
    let dir = if up { 1 } else { -1 };
    for i in 1..=channels {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let f = step(from, dir * i);
        // The lock per channel only: playback may retune in between.
        let mut s = shared();
        let Some(t) = s.tuner.as_mut() else { break };
        if t.probe(f)? {
            t.tune(f)?;
            WANT.store(f, Ordering::SeqCst);
            return Ok(Some(f));
        }
    }
    let mut s = shared();
    if let Some(t) = s.tuner.as_mut() {
        t.tune(from)?;
    }
    WANT.store(from, Ordering::SeqCst);
    Ok(None)
}

/// Scans the whole band and returns the channels that carry a station, in
/// order. `progress` gets each channel as it is probed. Afterwards the tuner is
/// back on the frequency playback wants.
pub fn scan(mut progress: impl FnMut(u16), cancel: &AtomicBool) -> io::Result<Vec<u16>> {
    let _lease = Lease::any()?;
    let mut found = Vec::new();
    let mut f = FREQ_MIN;
    while f <= FREQ_MAX && !cancel.load(Ordering::Relaxed) {
        progress(f);
        {
            let mut s = shared();
            let Some(t) = s.tuner.as_mut() else { break };
            if t.probe(f)? {
                found.push(f);
            }
        }
        f += STEP;
    }
    let back = WANT.load(Ordering::SeqCst);
    let mut s = shared();
    if let Some(t) = s.tuner.as_mut() {
        t.tune(back)?;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trip() {
        assert_eq!(uri(9860), "emilia-fm://9860");
        assert_eq!(parse_uri("emilia-fm://9860"), Some(9860));
        assert_eq!(parse_uri("emilia-fm://9860/"), Some(9860));
        // Off the raster: snapped onto it.
        assert_eq!(parse_uri("emilia-fm://9864"), Some(9860));
        assert_eq!(parse_uri("emilia-fm://9865"), Some(9870));
    }

    #[test]
    fn parse_uri_rejects_others() {
        assert_eq!(parse_uri("http://9860"), None);
        assert_eq!(parse_uri("emilia-fm://"), None);
        assert_eq!(parse_uri("emilia-fm://abc"), None);
        assert_eq!(parse_uri("emilia-fm://8000"), None); // below the band
        assert_eq!(parse_uri("emilia-fm://70000"), None); // not a u16
        assert_eq!(parse_uri("emilia-fm:9860"), None);
    }

    #[test]
    fn snap_to_band_and_raster() {
        assert_eq!(snap(0), FREQ_MIN);
        assert_eq!(snap(20000), FREQ_MAX);
        assert_eq!(snap(8754), 8750);
        assert_eq!(snap(8755), 8760);
        assert_eq!(snap(10799), FREQ_MAX);
    }

    #[test]
    fn labels() {
        assert_eq!(mhz(9860), "98.6");
        assert_eq!(mhz(8750), "87.5");
        assert_eq!(mhz(10800), "108.0");
        assert_eq!(label(10000), "100.0 MHz");
    }

    #[test]
    fn step_wraps_around_the_band() {
        assert_eq!(step(9860, 1), 9870);
        assert_eq!(step(9860, -1), 9850);
        assert_eq!(step(FREQ_MAX, 1), FREQ_MIN);
        assert_eq!(step(FREQ_MIN, -1), FREQ_MAX);
        // A full turn comes back.
        let n = i32::from((FREQ_MAX - FREQ_MIN) / STEP) + 1;
        assert_eq!(step(9860, n), 9860);
    }

    #[test]
    fn ioctl_numbers_match_the_driver() {
        // aarch64/x86_64: size field 8 (a pointer), as fmprobe measured on plinius.
        if std::mem::size_of::<usize>() == 8 {
            assert_eq!(IOCTL_POWERUP, 0xc008_f500);
            assert_eq!(IOCTL_TUNE, 0xc008_f502);
            assert_eq!(IOCTL_SOFT_MUTE_TUNE, 0xc008_f53f);
        }
        assert_eq!(std::mem::size_of::<TuneParm>(), 8);
        assert_eq!(std::mem::size_of::<SoftMuteTune>(), 12);
    }
}
