# Emilia – Architektur & Roadmap

Adaptiver Musik-, Podcast- und Streaming-Player für Linux-Desktops und
Phosh-Smartphones (Librem 5, PinePhone, FuriPhone), **GTK4 + libadwaita**,
geschrieben in **Rust** mit **relm4**. Was die App kann, steht im
[README](README.md); wie man sie baut, in [BUILDING.md](BUILDING.md). Diese
Seite hält die Designprämissen, den Stand der Architektur und die offenen
Vorhaben fest.

## Designprämissen

- **Ein adaptives UI** für Hochformat (schmal) und Desktop:
  `Adw::NavigationSplitView`, das auf dem Phone kollabiert; Seiten als
  relm4-Components, Wiedergabe zentral im Root-Component.
- **Schwache Hardware**: Rust, `rusqlite` (bundled SQLite) statt Server-DB,
  Scannen und Online-Anreicherung in Hintergrund-Workern, feste
  Speicherbudgets für lange Listen und Wellenformen.
- **Hörspiel-lastige Bibliotheken**: lange Tracks, über Tage gehört →
  Resume-Position pro Track/Episode und der Dateisystem-Browser als
  gleichwertige erste Ansicht (Tags sind oft lückenhaft).
- **Dateien werden nie beschrieben**: Tags, Cover, Lyrics und Online-Metadaten
  landen ausschließlich in der SQLite-DB und im XDG-Cache.
- **Opt-in für alles Netzwerkige**: Online-Metadaten, MCP-Server, Sync und
  Nextcloud sind standardmäßig aus.

## Tech-Stack

| Aufgabe                     | Crate / Lib                                          |
|-----------------------------|------------------------------------------------------|
| UI                          | `relm4`, `relm4-components`, `gtk4`, `libadwaita`    |
| Audio                       | `gstreamer` (`playbin3`, `equalizer-10bands`, gapless/crossfade) |
| Metadaten lesen             | `lofty`                                              |
| Bibliotheks-Index           | `rusqlite` (bundled)                                 |
| Lockscreen / Medientasten   | `mpris-server` (zbus)                                |
| HTTP-Client                 | `ureq`                                               |
| TLS / Sync / MCP-Server     | `rustls` 0.23 (ring), `rcgen`, eigener HTTP-Server (`core::http`) |
| MCP-SDK-Backend (optional)  | `rmcp`, `axum`, `tokio` – hinter dem Cargo-Feature `mcp-sdk` |
| QR-Code                     | `qrcode` (erzeugen), `rqrr` (Kamera-Scan)            |
| Tray                        | `ksni`, `x11rb` (Skip-Taskbar unter X11)             |
| i18n                        | `gettext-rs`, Extraktion mit `xtr`                   |

## Architektur (Stand 2026-09)

```
src/
  main.rs       Adw::Application, Panic-Hook → tracing, i18n-Init
  model.rs      Datenmodelle
  i18n.rs       gettext-Helfer (gettext_f, ngettext_n, gettext_noop)
  ui/           Root-Component `App` (app.rs + app_*.rs nach Domäne) und
                eigenständige Seiten-Components (podcasts_page, stream_page,
                yt_page, sync_page, cloud_page, stats_page, setup)
  core/         GTK-freie Logik: db/ (SQLite, Submodule je Domäne), player,
                scanner, online, lyrics, podcast, recorder, webdav, sync/,
                mcp/ (Tool-Schicht + zwei Backends), mpris, youtube, …
```

- **Root-Component `App`**: Navigation, Player-Leiste, Warteschlange und
  Wiedergabe (lokal, remote, Podcast, Stream, YouTube) bleiben bewusst im
  Root, weil sie den einen Player, MPRIS, den Tick und die Statistik teilen.
  Die flache `Msg`-Enum ist domänenweise in Sub-Enums gegliedert (`Playlist`,
  `Memo`, `Design`, `Tray`, `Sort`, `Eq`, `Source`, `McpSetting`, …), jede mit
  einem `update_<domain>` im Modul der Domäne. `view!` und das `App`-Literal
  bleiben absichtlich am Stück (deklarativ, keine Logik).
- **Datenbank**: eine Datei, WAL, Migrationen per Spalten-Probe plus
  `PRAGMA user_version`; jeder Worker öffnet eine eigene Verbindung.
- **Sicherheitsmodell**: MCP nur mit Bearer-Token, lokal gebunden, im
  LAN-Modus TLS; Geräte-Sync mit selbstsigniertem Zertifikat und
  SPKI-Fingerprint-Pinning per QR-Code; Passwörter und API-Keys im
  Secret Service.
- **Auslieferung**: signiertes Flatpak-OSTree-Repo für x86_64 (Desktop-
  Manifest) und aarch64 (Phone-Manifest), Module geteilt unter `flatpak/`.

## Roadmap

Erledigt: Bibliothek (Dateien/Interpreten/Alben/Singles/Kompilationen/
Konzerte/Hörbücher/Favoriten/Playlists), Resume, Equalizer-Kaskade mit
Ausgabeprofilen, MPRIS, Podcasts, Internetradio mit Timeshift-Recorder und
Wellenform-Editor, Sprachmemos, YouTube, Nextcloud/WebDAV als Quelle,
Geräte-Sync, MCP-Server, Lyrics/Karaoke, Sleep-Timer, Design-Seite mit Tray,
12 Sprachen, Flatpak-Repo.

Offen / Ideen:

- [ ] Weitere Streaming-Plattformen über yt-dlp (SoundCloud, Bandcamp,
      Mixcloud) – Plandokument liegt lokal unter `docs/` (nicht im Repo)
- [ ] Subsonic/Navidrome oder Jellyfin als Server-Backend (nach Nextcloud die
      nächste Remote-Quelle; Entscheidung noch offen)
- [ ] UI-Logik weiter testbar machen (reine Funktionen aus den Handlern
      ziehen); Integrationstests für Wiedergabe-Übergänge
- [ ] Distro-Pakete (Mobian/postmarketOS) zusätzlich zum Flatpak
