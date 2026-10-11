//! "Play on": the header cast button with its device popover. Lists this
//! device plus the network speakers (DLNA renderers, Sonos rooms) found by an
//! SSDP search that runs whenever the popover opens, and holds the speaker's
//! volume while casting. The playback hand-over itself lives in the player
//! (`crate::core::player`'s cast route).

use relm4::{ComponentSender, adw, gtk};

use adw::prelude::*;

use crate::core::cast::{self, Device, Kind};
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, Msg};

/// How long the SSDP search listens for answers.
const SEARCH_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);

/// `Msg` sub-enum of the cast picker.
#[derive(Debug)]
pub(crate) enum CastMsg {
    /// Search the LAN for renderers (popover opened / "search again").
    Search,
    /// The search finished.
    Found(Vec<Device>),
    /// Row picked: `None` = this device, `Some(i)` = `devices[i]`.
    Select(Option<usize>),
    /// Speaker volume slider moved (0.0–1.0).
    Volume(f64),
    /// Re-read the session state (the speaker's volume arrives a moment
    /// after the session starts).
    Refresh,
}

impl App {
    /// Builds the popover onto the header cast `btn`. Called once from
    /// `finish_init`.
    pub(crate) fn setup_cast_button(
        &mut self,
        btn: &gtk::MenuButton,
        sender: &ComponentSender<Self>,
    ) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(8)
            .margin_end(8)
            .width_request(260)
            .build();

        let head = gtk::Box::builder().spacing(6).build();
        head.append(
            &gtk::Label::builder()
                .label(gettext("Play on"))
                .halign(gtk::Align::Start)
                .hexpand(true)
                .css_classes(["heading"])
                .build(),
        );
        let spinner = gtk::Spinner::new();
        head.append(&spinner);
        let again = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text(gettext("Search again"))
            .css_classes(["flat", "circular"])
            .build();
        {
            let sender = sender.clone();
            again.connect_clicked(move |_| sender.input(Msg::Cast(CastMsg::Search)));
        }
        head.append(&again);
        content.append(&head);

        let status = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .build();
        content.append(&status);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        {
            let sender = sender.clone();
            list.connect_row_activated(move |_, row| {
                let idx = row.index();
                let pick = if idx <= 0 {
                    None
                } else {
                    Some(idx as usize - 1)
                };
                sender.input(Msg::Cast(CastMsg::Select(pick)));
            });
        }
        content.append(&list);

        // Speaker volume, only while casting.
        let volume_box = gtk::Box::builder().spacing(6).margin_top(4).build();
        volume_box.append(&gtk::Image::from_icon_name("audio-volume-medium-symbolic"));
        let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        volume.set_hexpand(true);
        volume.set_draw_value(false);
        {
            let sender = sender.clone();
            let guard = self.cast.volume_guard.clone();
            volume.connect_value_changed(move |s| {
                if !guard.get() {
                    sender.input(Msg::Cast(CastMsg::Volume(s.value())));
                }
            });
        }
        volume_box.append(&volume);
        content.append(&volume_box);

        let popover = gtk::Popover::builder().child(&content).build();
        {
            let sender = sender.clone();
            popover.connect_show(move |_| sender.input(Msg::Cast(CastMsg::Search)));
        }
        btn.set_popover(Some(&popover));

        self.cast.button = btn.clone();
        self.cast.list = list;
        self.cast.status = status;
        self.cast.spinner = spinner;
        self.cast.volume_box = volume_box;
        self.cast.volume = volume;
        self.sync_cast_ui();
    }

    pub(crate) fn update_cast(&mut self, msg: CastMsg, sender: &ComponentSender<Self>) {
        match msg {
            CastMsg::Search => {
                if self.cast.searching {
                    return;
                }
                self.cast.searching = true;
                self.cast.spinner.set_spinning(true);
                self.sync_cast_ui();
                let input = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let found = cast::discover(SEARCH_WAIT);
                    input.emit(Msg::Cast(CastMsg::Found(found)));
                });
            }
            CastMsg::Found(mut devices) => {
                self.cast.searching = false;
                self.cast.searched = true;
                self.cast.spinner.set_spinning(false);
                // Never offer this app's own renderer: casting to ourselves
                // would loop.
                if let Some(own) = self.dmr.handle.as_ref().map(|h| h.id().to_string()) {
                    devices.retain(|d| d.id != own);
                }
                if let Some(own) = self.dmr.airplay.as_ref().map(|h| h.id().to_string()) {
                    devices.retain(|d| d.id != own);
                }
                // Keep the active speaker listed even if it missed this round.
                if let Some(cur) = self.player.cast_device()
                    && !devices.iter().any(|d| d.id == cur.id)
                {
                    devices.insert(0, cur);
                }
                self.cast.devices = devices;
                self.sync_cast_ui();
            }
            CastMsg::Select(pick) => {
                let current = self.player.cast_device().map(|d| d.id);
                match pick.and_then(|i| self.cast.devices.get(i).cloned()) {
                    None => {
                        if current.is_some() {
                            self.player.stop_cast();
                        }
                    }
                    Some(dev) => {
                        if current.as_deref() != Some(dev.id.as_str()) {
                            self.player.start_cast(dev);
                        }
                    }
                }
                self.cast.button.popdown();
                self.sync_cast_ui();
                // The speaker's volume arrives a moment after the session starts.
                let input = sender.input_sender().clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(1500),
                    move || {
                        input.emit(Msg::Cast(CastMsg::Refresh));
                    },
                );
            }
            CastMsg::Volume(v) => self.player.set_cast_volume(v),
            CastMsg::Refresh => self.sync_cast_ui(),
        }
    }

    /// Re-creates the rows: this device first, then the speakers found.
    fn rebuild_cast_list(&self) {
        let list = &self.cast.list;
        while let Some(row) = list.first_child() {
            list.remove(&row);
        }
        let active = self.player.cast_device().map(|d| d.id);
        let local_name = gettext("This device");
        list.append(&cast_row(
            &local_name,
            None,
            "audio-x-generic-symbolic",
            active.is_none(),
        ));
        for dev in &self.cast.devices {
            let icon = match dev.kind {
                Kind::Sonos | Kind::Dlna | Kind::Cast | Kind::AirPlay => "audio-speakers-symbolic",
            };
            let subtitle = match dev.kind {
                Kind::Sonos => format!("Sonos · {}", dev.host),
                Kind::Cast if dev.model.is_empty() => format!("Google Cast · {}", dev.host),
                Kind::Cast => format!("Google Cast · {} · {}", dev.model, dev.host),
                Kind::AirPlay => format!("AirPlay · {}", dev.host),
                Kind::Dlna => {
                    if dev.model.is_empty() {
                        dev.host.clone()
                    } else {
                        format!("{} · {}", dev.model, dev.host)
                    }
                }
            };
            list.append(&cast_row(
                &dev.name,
                Some(&subtitle),
                icon,
                active.as_deref() == Some(dev.id.as_str()),
            ));
        }
    }

    /// Button highlight + tooltip, popover status line, rows and volume.
    pub(crate) fn sync_cast_ui(&self) {
        let dev = self.player.cast_device();
        let airplay = self.dmr.airplay_now.as_ref();
        if let Some(now) = airplay {
            self.cast.button.add_css_class("emilia-casting");
            let text = if now.is_empty() {
                gettext("Receiving via AirPlay")
            } else {
                gettext_f("Receiving via AirPlay: {title}", &[("title", now)])
            };
            self.cast.button.set_tooltip_text(Some(&text));
        } else if let Some(d) = &dev {
            self.cast.button.add_css_class("emilia-casting");
            self.cast
                .button
                .set_tooltip_text(Some(&gettext_f("Playing on {name}", &[("name", &d.name)])));
        } else {
            self.cast.button.remove_css_class("emilia-casting");
            self.cast.button.set_tooltip_text(Some(&gettext("Play on")));
        }
        let status = if let Some(now) = airplay.filter(|_| dev.is_none() && !self.cast.searching) {
            if now.is_empty() {
                gettext("Receiving via AirPlay")
            } else {
                gettext_f("Receiving via AirPlay: {title}", &[("title", now)])
            }
        } else if self.cast.searching {
            gettext("Searching for speakers in the network…")
        } else if let Some(d) = &dev {
            gettext_f("Playing on {name}", &[("name", &d.name)])
        } else if self.cast.searched && self.cast.devices.is_empty() {
            gettext(
                "No speakers found. DLNA/UPnP renderers, Sonos, Google Cast and AirPlay devices in the same network are supported.",
            )
        } else {
            String::new()
        };
        self.cast.status.set_visible(!status.is_empty());
        self.cast.status.set_label(&status);
        let vol = self.player.cast_volume();
        self.cast
            .volume_box
            .set_visible(dev.is_some() && vol.is_some());
        if let Some(v) = vol {
            self.cast.volume_guard.set(true);
            self.cast.volume.set_value(v);
            self.cast.volume_guard.set(false);
        }
        self.rebuild_cast_list();
    }
}

fn cast_row(title: &str, subtitle: Option<&str>, icon: &str, active: bool) -> gtk::ListBoxRow {
    let row = adw::ActionRow::builder()
        .title(gtk::glib::markup_escape_text(title).as_str())
        .activatable(true)
        .build();
    if let Some(s) = subtitle {
        row.set_subtitle(gtk::glib::markup_escape_text(s).as_str());
    }
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    if active {
        row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
    }
    row.upcast()
}
