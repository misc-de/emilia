//! Sleep timer: the header "zzz" button with its preset popover, the
//! once-per-second countdown and the volume fade-out toward the deadline.
//! Kept separate from `app_playback` so the transport logic stays focused.

use relm4::{gtk, ComponentSender};

use gtk::prelude::*;

use crate::i18n::gettext;
use crate::ui::app::{App, Msg, SleepChoice};

/// Length of the gentle fade-out at the end of a timed sleep, in seconds. The
/// output volume ramps from full down to silent over the final two minutes so
/// the music tapers off instead of cutting out abruptly.
pub(crate) const SLEEP_FADE_S: f64 = 120.0;

impl App {
    /// Builds the sleep-timer popover (status line + presets) onto the header
    /// `btn` and remembers the button + status label for later updates. Called
    /// once from `finish_init`.
    pub(crate) fn setup_sleep_button(
        &mut self,
        btn: &gtk::MenuButton,
        sender: &ComponentSender<Self>,
    ) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .width_request(200)
            .build();

        let title = gtk::Label::builder()
            .label(gettext("Sleep timer"))
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        content.append(&title);

        let status = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption"])
            .margin_bottom(4)
            .build();
        content.append(&status);

        // Timed presets (each fades out over the final stretch) + "end of track".
        let presets: [(String, SleepChoice); 5] = [
            (format!("15 {}", gettext("min")), SleepChoice::Minutes(15)),
            (format!("30 {}", gettext("min")), SleepChoice::Minutes(30)),
            (format!("45 {}", gettext("min")), SleepChoice::Minutes(45)),
            (format!("60 {}", gettext("min")), SleepChoice::Minutes(60)),
            (gettext("End of current track"), SleepChoice::EndOfTrack),
        ];
        for (label, choice) in presets {
            let b = gtk::Button::builder()
                .label(label)
                .css_classes(["flat"])
                .build();
            b.child().and_downcast::<gtk::Label>().inspect(|l| {
                l.set_xalign(0.0);
            });
            let sender = sender.clone();
            b.connect_clicked(move |_| sender.input(Msg::SetSleepTimer(choice)));
            content.append(&b);
        }

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let off = gtk::Button::builder()
            .label(gettext("Off"))
            .css_classes(["flat"])
            .build();
        off.child().and_downcast::<gtk::Label>().inspect(|l| {
            l.set_xalign(0.0);
        });
        {
            let sender = sender.clone();
            off.connect_clicked(move |_| sender.input(Msg::SetSleepTimer(SleepChoice::Off)));
        }
        content.append(&off);

        let popover = gtk::Popover::builder().child(&content).build();
        btn.set_popover(Some(&popover));

        self.sleep.button = btn.clone();
        self.sleep.status_label = status;
        self.refresh_sleep_ui();
    }

    /// Arms or clears the sleep timer from a popover choice. Re-arming resets any
    /// partial fade back to full volume; the popover closes afterwards.
    pub(crate) fn on_set_sleep_timer(&mut self, choice: SleepChoice) {
        match choice {
            SleepChoice::Off => {
                self.sleep.remaining_s = None;
                self.sleep.until_track_end = false;
            }
            SleepChoice::Minutes(m) => {
                self.sleep.remaining_s = Some(m * 60);
                self.sleep.until_track_end = false;
            }
            SleepChoice::EndOfTrack => {
                self.sleep.remaining_s = None;
                self.sleep.until_track_end = true;
            }
        }
        // Undo any fade left over from a previous timer.
        self.player.set_volume(1.0);
        self.refresh_sleep_ui();
        if let Some(pop) = self.sleep.button.popover() {
            pop.popdown();
        }
    }

    /// Mirrors the current sleep state onto the header icon (armed tint) and the
    /// popover status line.
    pub(crate) fn refresh_sleep_ui(&self) {
        let armed = self.sleep.remaining_s.is_some() || self.sleep.until_track_end;
        if armed {
            self.sleep.button.add_css_class("sleep-armed");
        } else {
            self.sleep.button.remove_css_class("sleep-armed");
        }
        let text = match self.sleep.remaining_s {
            Some(rem) => {
                let (m, s) = (rem / 60, rem % 60);
                format!("{} {m}:{s:02}", gettext("Pauses in"))
            }
            None if self.sleep.until_track_end => gettext("Pauses at end of current track"),
            None => gettext("Off"),
        };
        self.sleep.status_label.set_text(&text);
    }

    /// One-second countdown step, driven from [`App::on_tick`] while playing.
    /// Counts down a timed sleep, fades the volume toward the deadline and fires
    /// the pause once it reaches zero. No-op when no timed sleep is armed.
    pub(crate) fn sleep_tick(&mut self) {
        let Some(rem) = self.sleep.remaining_s else {
            return;
        };
        let rem = rem - 1;
        if rem <= 0 {
            self.fire_sleep_timer();
            return;
        }
        self.sleep.remaining_s = Some(rem);
        // Gentle fade over the final stretch (leave full volume before that).
        if (rem as f64) < SLEEP_FADE_S {
            self.player.set_volume((rem as f64 / SLEEP_FADE_S).clamp(0.0, 1.0));
        }
        self.refresh_sleep_ui();
    }

    /// The timed sleep reached zero: persist resume points, pause playback,
    /// restore full volume (so the next manual play is not silent) and disarm.
    fn fire_sleep_timer(&mut self) {
        self.save_resume();
        if self.podcasts.playing_episode_url.is_some() {
            self.save_episode_progress();
        }
        self.player.pause();
        self.player.set_volume(1.0);
        self.mini.playing = false;
        self.mini.loading = false;
        self.sleep.remaining_s = None;
        self.sleep.until_track_end = false;
        self.mpris.set_playing(false);
        self.refresh_queue_icons();
        self.refresh_sleep_ui();
        self.toast(&gettext("Sleep timer: playback paused"));
    }

    /// Called from [`App::on_track_finished`] when a "stop at end of track" sleep
    /// is armed: stop instead of advancing. Returns `true` when it handled the
    /// end (so the caller skips its normal advance), `false` otherwise.
    pub(crate) fn sleep_stop_at_track_end(&mut self) -> bool {
        if !self.sleep.until_track_end {
            return false;
        }
        // Count the finished track as fully listened, then stop.
        self.finalize_play_session(true);
        if let Some(path) = self.transport.playing_path.take() {
            let _ = self.library.set_resume_path(&path.to_string_lossy(), 0);
        }
        *self.transport.close_resume.borrow_mut() = None;
        self.player.stop();
        self.mini.playing = false;
        self.mini.loading = false;
        self.sleep.until_track_end = false;
        self.podcasts.playing_episode_url = None;
        self.mpris.set_playing(false);
        self.refresh_queue_icons();
        self.refresh_sleep_ui();
        self.toast(&gettext("Sleep timer: playback stopped"));
        true
    }
}
