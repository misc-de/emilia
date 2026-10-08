//! Desktop integration of the transport: MPRIS metadata, desktop URI opens,
//! desktop seeks and the MPRIS command handler. Split out of
//! [`crate::ui::app_playback`] – pure reordering, the methods remain inherent
//! `impl App` methods.

use gtk::prelude::GtkWindowExt;
use relm4::{adw, gtk};

use crate::model::Track;
use crate::ui::app::App;
use crate::ui::app_playback::{SeekTarget, absolute_seek, relative_seek};

impl App {
    /// Sends the metadata of the running track to the MPRIS service
    /// (lock screen). The cover – if present – is added best effort.
    pub(crate) fn update_mpris_metadata(&self, path: &std::path::Path, track: Option<&Track>) {
        let (title, artist, album, length) = match track {
            Some(t) => (
                t.title.clone(),
                t.artist.clone(),
                t.album.clone(),
                t.duration_ms,
            ),
            None => (Self::track_display_name(path), None, None, None),
        };
        let art = track.and_then(|t| self.playing_cover_path(t));
        self.mpris.set_metadata(
            self.transport.queue_pos,
            &title,
            artist.as_deref(),
            album.as_deref(),
            length,
            art.as_deref(),
        );
        // Keep the lock-screen shuffle/repeat in sync (e.g. repeat restored from
        // settings at startup, which predates the async MPRIS player being ready).
        self.mpris.set_shuffle(self.transport.shuffle);
        self.mpris.set_repeat(self.transport.repeat);
    }

    /// `OpenUri` from the desktop (a file manager's "open with", a browser
    /// handing over a stream). Local paths go through the normal play path — a
    /// folder becomes a queue, a file a single track; network URLs take the
    /// episode route, which keeps position/resume bookkeeping keyed by the URL.
    /// Anything else is refused, matching the advertised `SupportedUriSchemes`.
    fn open_uri_from_desktop(&mut self, uri: &str) {
        let scheme = uri.split_once(':').map(|(s, _)| s).unwrap_or("");
        match scheme.to_ascii_lowercase().as_str() {
            "file" => {
                let Ok((path, _)) = gtk::glib::filename_from_uri(uri) else {
                    tracing::warn!("MPRIS OpenUri: not a local path: {uri}");
                    return;
                };
                if !path.exists() {
                    tracing::warn!("MPRIS OpenUri: no such path: {}", path.display());
                    return;
                }
                let is_dir = path.is_dir();
                self.play_path(&path.to_string_lossy(), is_dir);
            }
            "http" | "https" => {
                // A known enclosure keeps its episode title; otherwise name it
                // after the last path segment, falling back to the URL.
                let title = self
                    .library
                    .episode_title_by_url(uri)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| {
                        uri.rsplit('/')
                            .next()
                            .and_then(|seg| seg.split(['?', '#']).next())
                            .filter(|seg| !seg.is_empty())
                            .unwrap_or(uri)
                            .to_owned()
                    });
                self.play_episode(uri, &title);
            }
            _ => tracing::warn!("MPRIS OpenUri: unsupported scheme in {uri}"),
        }
    }

    /// Length of what is playing, from the pipeline (authoritative) or the last
    /// value the tick saw. `None` for a live stream, which has no end to run
    /// past.
    pub(crate) fn playing_length_ms(&self) -> Option<i64> {
        self.player
            .duration_ms()
            .filter(|&d| d > 0)
            .or(Some(self.mini.track_duration_ms).filter(|&d| d > 0))
    }

    /// Seeks on behalf of the desktop and reports the jump back to it.
    pub(crate) fn seek_from_desktop(&mut self, target_ms: i64) {
        if self.player.seek_ms(target_ms).is_ok() {
            self.mini.position_ms = target_ms;
            self.mpris.set_position(target_ms);
            self.mpris.seeked(target_ms);
        }
    }

    /// Handle a desktop / lock-screen MPRIS command (media keys, etc.).
    pub(crate) fn handle_mpris(
        &mut self,
        root: &adw::ApplicationWindow,
        cmd: crate::core::mpris::MprisCommand,
    ) {
        use crate::core::mpris::MprisCommand as M;
        match cmd {
            // All three go through the player bar's own toggle: it is the only
            // path that knows how to (re)load when the app has a current track
            // but the pipeline is empty — reporting "playing" for an empty
            // pipeline is exactly what the desktop must not be told.
            M::PlayPause => self.on_toggle_play(),
            M::Play => {
                if !self.mini.playing {
                    self.on_toggle_play();
                }
            }
            M::Pause => {
                if self.mini.playing {
                    self.on_toggle_play();
                }
            }
            M::Next => self.skip_next(),
            M::Prev => self.skip_prev(),
            // Car head units send Stop over Bluetooth (AVRCP) when they
            // disconnect, and that must not throw away the track: anything
            // with a position just pauses, so it continues on the next output
            // (headset, speaker) with Play. Only a station or live stream,
            // which has nothing to keep, is torn down.
            M::Stop
                if self.streaming.playing_stream.is_none()
                    && self.youtube.playing_live.is_none() =>
            {
                if self.mini.playing {
                    self.on_toggle_play();
                }
            }
            M::Stop => {
                self.save_resume();
                self.finalize_play_session(false);
                self.player.stop();
                self.mini.playing = false;
                self.transport.playing_path = None;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.mpris.set_stopped();
                // The tick no longer runs, so the last position would stay
                // frozen in the property — a stopped player is at 0.
                self.mpris.set_position(0);
                self.refresh_queue_icons();
            }
            M::Raise => root.present(),
            M::SeekBy(offset_us) => {
                let cur = self.player.position_ms().unwrap_or(self.mini.position_ms);
                match relative_seek(cur, offset_us / 1000, self.playing_length_ms()) {
                    SeekTarget::To(ms) => self.seek_from_desktop(ms),
                    SeekTarget::Next => self.skip_next_item(),
                    SeekTarget::Ignore => {}
                }
            }
            M::SetPosition(pos_us) => {
                match absolute_seek(pos_us / 1000, self.playing_length_ms()) {
                    SeekTarget::To(ms) => self.seek_from_desktop(ms),
                    SeekTarget::Next | SeekTarget::Ignore => {}
                }
            }
            M::SetShuffle(on) => {
                if self.transport.shuffle != on {
                    self.transport.shuffle = on;
                    if on {
                        self.rebuild_shuffle_order();
                    }
                }
                self.mpris.set_shuffle(self.transport.shuffle);
            }
            M::SetRepeat(on) => {
                if self.transport.repeat != on {
                    self.transport.repeat = on;
                    let _ = self
                        .library
                        .set_setting("repeat", if on { "1" } else { "0" });
                }
                self.mpris.set_repeat(self.transport.repeat);
            }
            M::OpenUri(uri) => self.open_uri_from_desktop(&uri),
            M::SetVolume(vol) => {
                self.player.set_master_volume(vol);
                // The MPRIS setter only calls back, so mirror the (clamped)
                // value into the property the desktop reads.
                self.mpris.set_volume(self.player.master_volume());
            }
        }
    }
}
