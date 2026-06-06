# Credits

Emilia stands on the shoulders of many open-source projects, libraries and
freely available data services. Heartfelt thanks to everyone who builds and
maintains them.

Emilia itself is free software, licensed under the **GNU General Public License
v3.0 or later** (`GPL-3.0-or-later`).

## Platform & runtime

- [GNOME Platform & SDK](https://www.gnome.org/) — the Flatpak runtime Emilia ships against.
- [GTK 4](https://www.gtk.org/) — the widget toolkit.
- [libadwaita](https://gitlab.gnome.org/GNOME/libadwaita) — adaptive GNOME UI patterns for desktop and phone.
- [GStreamer](https://gstreamer.freedesktop.org/) — audio playback, stream recording and the camera capture pipeline.
- [PipeWire](https://pipewire.org/) — audio and, via the camera portal, webcam access for QR pairing.
- [xdg-desktop-portal](https://github.com/flatpak/xdg-desktop-portal) — sandboxed access to the camera and the file chooser.
- [Flatpak](https://flatpak.org/) — packaging and distribution.

## Rust libraries

**UI & application framework**
- [Relm4](https://relm4.org/) — `relm4`, `relm4-components`
- [gtk4-rs / gtk-rs](https://gtk-rs.org/) — `gtk4` and the GNOME Rust bindings
- [libadwaita-rs](https://world.pages.gitlab.gnome.org/Rust/libadwaita-rs/) — `libadwaita`

**Media**
- [gstreamer-rs](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs) — `gstreamer`, `gstreamer-app`
- [Lofty](https://github.com/Serial-ATA/lofty-rs) — audio tag reading/writing
- [rqrr](https://github.com/WanzenBug/rqrr) — pure-Rust QR decoding (camera scanner)
- [qrcode](https://github.com/kennytm/qrcode-rust) — QR generation (device pairing)

**Data & storage**
- [rusqlite](https://github.com/rusqlite/rusqlite) + [SQLite](https://www.sqlite.org/) — the library database (bundled)
- [serde](https://serde.rs/) / [serde_json](https://github.com/serde-rs/json)
- [quick-xml](https://github.com/tafia/quick-xml) and [rss](https://github.com/rust-syndication/rss) — podcast feed parsing
- [dirs](https://github.com/dirs-dev/dirs-rs)

**Networking & security**
- [ureq](https://github.com/algesten/ureq) — HTTP client for online metadata
- [rustls](https://github.com/rustls/rustls) + [ring](https://github.com/briansmith/ring) — TLS for the LAN sync server/client and online requests
- [rcgen](https://github.com/rustls/rcgen) — self-signed certificates for device pairing
- [x509-parser](https://github.com/rusticata/x509-parser)
- [RustCrypto: sha2](https://github.com/RustCrypto/hashes), [base64](https://github.com/marshallpierce/rust-base64), [getrandom](https://github.com/rust-random/getrandom)
- [httparse](https://github.com/seanmonstar/httparse), [percent-encoding](https://github.com/servo/rust-url)

**System integration**
- [zbus](https://github.com/dbus2/zbus) — D-Bus (MPRIS and the camera portal)
- [mpris-server](https://github.com/SeaDve/mpris-server) — MPRIS media controls / lock-screen integration
- [gettext-rs](https://github.com/gettext-rs/gettext-rs) — translations
- [async-channel](https://github.com/smol-rs/async-channel)

**Diagnostics**
- [tracing](https://github.com/tokio-rs/tracing) — `tracing`, `tracing-subscriber`
- [anyhow](https://github.com/dtolnay/anyhow)

The complete, authoritative list with exact versions and licenses lives in
[`Cargo.toml`](Cargo.toml) and [`Cargo.lock`](Cargo.lock).

## Bundled tools

- [yt-dlp](https://github.com/yt-dlp/yt-dlp) — powers the YouTube music source. A pinned, checksum-verified binary is shipped; the app may download a newer version on demand.

## Online data sources

Emilia enriches your local library and discovers content through these free
services. Please respect their respective terms of use.

- [MusicBrainz](https://musicbrainz.org/) — release/album metadata
- [Cover Art Archive](https://coverartarchive.org/) — album cover art
- [AcoustID](https://acoustid.org/) + [Chromaprint](https://acoustid.org/chromaprint) (`fpcalc`) — audio-fingerprint track recognition
- [Deezer](https://www.deezer.com/) — artist photos
- [fanart.tv](https://fanart.tv/) — artist/album image galleries
- [Radio-Browser](https://www.radio-browser.info/) — internet radio station directory
- [Apple iTunes Search API](https://podcasts.apple.com/) — podcast directory search

## Icons & design

- [Adwaita icon theme](https://gitlab.gnome.org/GNOME/adwaita-icon-theme) and the GNOME symbolic-icon style. Emilia ships a few custom symbolic icons in the same style under [`data/icons/`](data/icons/).

---

If you maintain a project listed here and would like the attribution corrected or
expanded — or if something is missing — please open an issue.
