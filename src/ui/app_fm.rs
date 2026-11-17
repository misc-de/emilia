//! FM radio transport (the page is [`crate::ui::fm_page`]): playing a
//! frequency through the normal player (`emilia-fm://<freq>`, see
//! [`crate::core::fm_src`]), retuning it live, and stepping through the saved
//! stations with next/previous — from the player bar, MPRIS and so the buttons
//! of a Bluetooth or wired headset.

use relm4::adw;
use relm4::prelude::*;

use crate::core::fm;
use crate::i18n::gettext;
use crate::ui::app::App;
use crate::ui::fm_page::FmInput;

/// FM playback state on `App`.
#[derive(Default)]
pub(crate) struct FmState {
    /// The frequency FM plays on while it is the current source (also while
    /// paused); `None` while anything else is.
    pub(crate) playing: Option<u16>,
}

#[derive(Debug)]
pub(crate) enum FmMsg {
    /// Play `freq`, or pause/resume if FM already plays there.
    Toggle(u16),
    /// Retune the playing tuner.
    Tune(u16),
    /// The saved stations changed (names shown while playing).
    StationsChanged,
}

impl App {
    pub(crate) fn update_fm(
        &mut self,
        msg: FmMsg,
        _root: &adw::ApplicationWindow,
        _sender: &ComponentSender<Self>,
    ) {
        match msg {
            FmMsg::Toggle(freq) => match self.fm.playing {
                Some(cur) if self.mini.playing => {
                    if cur == freq {
                        self.flip_playing();
                    } else {
                        self.fm_retune(freq);
                    }
                }
                // Nothing on FM, or FM paused (= stopped): start it fresh.
                _ => self.play_fm(freq),
            },
            FmMsg::Tune(freq) => {
                if self.fm.playing.is_some() {
                    self.fm_retune(freq);
                }
            }
            FmMsg::StationsChanged => {
                if let Some(f) = self.fm.playing {
                    self.fm_show(f);
                }
            }
        }
    }

    /// The name FM shows for `freq`: the saved station's, else the frequency.
    fn fm_title(&self, freq: u16) -> String {
        self.library
            .fm_station_at(freq)
            .ok()
            .flatten()
            .map(|s| s.name)
            .unwrap_or_else(|| fm::label(freq))
    }

    /// Bar and MPRIS show `freq`.
    fn fm_show(&mut self, freq: u16) {
        let title = self.fm_title(freq);
        let label = fm::label(freq);
        // Artist line: the frequency under a station name, else "FM radio".
        let artist = if title == label {
            gettext("FM radio")
        } else {
            label
        };
        self.mini.now_playing = Some(title.clone());
        self.mpris
            .set_metadata(0, &title, Some(&artist), None, None, None);
    }

    /// Starts FM on `freq`.
    pub(crate) fn play_fm(&mut self, freq: u16) {
        let freq = fm::snap(freq);
        self.save_resume();
        self.save_episode_progress();
        self.finalize_play_session(false);
        self.stop_recorder();
        match self.player.play_fm(freq) {
            Ok(()) => {
                self.fm.playing = Some(freq);
                self.mini.current_album = None;
                self.mini.playing = true;
                self.mini.loading = false;
                self.transport.playing_path = None;
                self.podcasts.playing_episode_url = None;
                self.streaming.playing_stream = None;
                self.streaming.stream_title = None;
                self.youtube.playing_video_id = None;
                self.youtube.playing_live = None;
                self.files.playing_remote = false;
                self.transport.queue.clear();
                self.transport.queue_pos = 0;
                self.mini.position_ms = 0;
                self.mini.track_duration_ms = 0;
                *self.transport.close_resume.borrow_mut() = None;
                self.library.set_fm_last_freq(freq);
                self.fm_show(freq);
                self.fm_page.emit(FmInput::Follow(freq));
                self.mpris.set_playing(true);
                self.set_chapters(Vec::new());
                self.settings.active_output =
                    crate::core::output::default_output().unwrap_or_default();
                self.apply_current_eq();
                self.refresh_queue_icons();
            }
            Err(e) => {
                tracing::error!("FM: cannot play {}: {e}", fm::label(freq));
                self.toast(&gettext("FM radio could not start"));
            }
        }
    }

    /// FM plays: move the tuner to `freq` without restarting playback.
    fn fm_retune(&mut self, freq: u16) {
        let freq = fm::snap(freq);
        if self.fm.playing == Some(freq) {
            return;
        }
        fm::request_tune(freq);
        self.fm.playing = Some(freq);
        // Saved where the frequency comes to rest (the page on releasing the
        // dial, `fm_step`), not for every channel the dial passes.
        self.fm_show(freq);
        self.sync_fm_page();
    }

    /// Next/previous while FM plays: the neighbouring saved station (cycling).
    /// Without saved stations a step goes to the neighbouring channel.
    pub(crate) fn fm_step(&mut self, dir: i32) {
        let Some(cur) = self.fm.playing else {
            return;
        };
        let stations = self.library.fm_stations().unwrap_or_default();
        let next = match next_station(&stations, cur, dir) {
            Some(f) => f,
            None => fm::step(cur, dir),
        };
        if self.mini.playing {
            self.fm_retune(next);
            self.library.set_fm_last_freq(next);
            self.fm_page.emit(FmInput::Follow(next));
        } else {
            self.play_fm(next);
        }
    }

    /// Pauses FM by stopping it: a paused tuner would stay switched on, and a
    /// broadcast is not resumed where it was anyway (the resume replays it).
    pub(crate) fn fm_pause(&mut self) {
        self.player.stop();
        self.mini.playing = false;
        self.mini.loading = false;
    }

    /// The player reported an error while FM plays: stop it and tell the page.
    pub(crate) fn fm_failed(&mut self) {
        self.finalize_play_session(false);
        self.player.stop();
        self.mini.playing = false;
        self.mpris.set_playing(false);
        self.refresh_queue_icons();
        self.fm_page.emit(FmInput::PlaybackFailed);
    }

    /// Pushes the FM playback state to the page (from `refresh_queue_icons`).
    pub(crate) fn sync_fm_page(&self) {
        self.fm_page.emit(FmInput::PlaybackStateChanged {
            active: self.fm.playing.is_some(),
            playing: self.mini.playing && self.fm.playing.is_some(),
        });
    }
}

/// The saved station `dir` places from `cur` in the user's order, cycling.
/// From a frequency that is not saved, the nearest saved one in that direction
/// (by frequency). `None` without saved stations.
fn next_station(stations: &[crate::model::FmStation], cur: u16, dir: i32) -> Option<u16> {
    if stations.is_empty() {
        return None;
    }
    let n = stations.len() as i32;
    if let Some(i) = stations.iter().position(|s| s.freq == cur) {
        let j = (i as i32 + dir).rem_euclid(n) as usize;
        return Some(stations[j].freq);
    }
    let mut freqs: Vec<u16> = stations.iter().map(|s| s.freq).collect();
    freqs.sort_unstable();
    if dir > 0 {
        freqs
            .iter()
            .copied()
            .find(|&f| f > cur)
            .or(freqs.first().copied())
    } else {
        freqs
            .iter()
            .rev()
            .copied()
            .find(|&f| f < cur)
            .or(freqs.last().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FmStation;

    fn st(id: i64, freq: u16) -> FmStation {
        FmStation {
            id,
            freq,
            name: String::new(),
        }
    }

    #[test]
    fn steps_through_saved_stations_in_list_order() {
        // List order, not frequency order.
        let list = [st(1, 10000), st(2, 8800), st(3, 9860)];
        assert_eq!(next_station(&list, 10000, 1), Some(8800));
        assert_eq!(next_station(&list, 9860, 1), Some(10000)); // wraps
        assert_eq!(next_station(&list, 10000, -1), Some(9860)); // wraps
        assert_eq!(next_station(&list, 8800, -1), Some(10000));
    }

    #[test]
    fn from_an_unsaved_frequency_to_the_nearest_saved() {
        let list = [st(1, 10000), st(2, 8800), st(3, 9860)];
        assert_eq!(next_station(&list, 9000, 1), Some(9860));
        assert_eq!(next_station(&list, 9000, -1), Some(8800));
        assert_eq!(next_station(&list, 10500, 1), Some(8800)); // wraps
        assert_eq!(next_station(&list, 8700, -1), Some(10000)); // wraps
        assert_eq!(next_station(&[], 9000, 1), None);
    }
}
