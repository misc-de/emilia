# Emilia – Musikplayer für Linux Phosh

Adaptiver Musik- & Hörspielplayer für Phosh-Smartphones (Librem 5, PinePhone &
Co.) auf **GTK4 + libadwaita**, geschrieben in **Rust** mit **relm4**.

## Zielplattform & Designprämissen

- **Phosh / GNOME Mobile**: adaptives UI, das mobil (Hochformat, schmal) und am
  Desktop funktioniert → `Adw::NavigationSplitView`, das auf dem Phone kollabiert.
- **Schwache Hardware**: niedriger RAM-/CPU-Verbrauch → Rust, `rusqlite` statt
  Server-DB, Scannen im Hintergrund-Worker.
- **Hörspiel-lastige Bibliothek** (siehe `Emilia-Musik`): lange Tracks, über Tage
  gehört → **Resume-Position pro Track** und **Dateisystem-Navigation** als
  gleichwertige erste Ansicht (Tags oft lückenhaft).
- **PipeWire/Wireplumber** als Audio-Stack → GStreamer `playbin3`.

## Tech-Stack

| Aufgabe              | Crate / Lib                                   |
|----------------------|-----------------------------------------------|
| UI-Framework         | `relm4`, `relm4-components`, `gtk4`, `libadwaita` |
| Audio                | `gstreamer`, `playbin3` + `equalizer-10bands` |
| Metadaten lesen      | `lofty` (Tags + Cover, viele Formate)         |
| Bibliotheks-Index    | `rusqlite` (bundled SQLite)                   |
| Lockscreen/Medientasten | `mpris-server` (zbus)                      |
| XDG-Pfade            | `dirs`                                         |

Vorhandene System-Libs (verifiziert): GTK 4.22.4, libadwaita 1.9.1,
GStreamer 1.28.3, gstreamer-player 1.28.3.
**Fehlt noch: Rust-Toolchain (`cargo`/`rustc`).**

## Modulstruktur

```
src/
  main.rs            App-Init, Adw::Application
  ui/
    app.rs           Root-Component (Adw::NavigationSplitView → kollabiert mobil)
    library.rs       Browser: Tabs Dateisystem | Interpreten | Alben
    player_bar.rs    Mini-Player unten + ausklappbarer Vollbild-Player
    queue.rs         Wiedergabeliste
    eq.rs            Equalizer-Editor (10 Bänder, Scope-Auswahl)
  core/
    scanner.rs       Verzeichnis-Scan, lofty-Metadaten → DB (Background-Worker)
    db.rs            rusqlite, Migrations
    player.rs        GStreamer-Wrapper, Playback-State, Position speichern
    eq_engine.rs     EQ-Auflösung (Kaskade) + live an GStreamer
    mpris.rs         MPRIS-Bridge
  model/
    track.rs  album.rs  artist.rs  eq_preset.rs
```

## Datenmodell (SQLite)

```sql
CREATE TABLE artist (id INTEGER PRIMARY KEY, name TEXT UNIQUE);
CREATE TABLE album  (id INTEGER PRIMARY KEY, title TEXT, artist_id INTEGER,
                     cover_path TEXT);
CREATE TABLE track (
  id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,      -- Dateisystem = verlässlichste Quelle
  title TEXT, track_no INTEGER,
  album_id INTEGER, artist_id INTEGER,
  duration_ms INTEGER,
  resume_ms INTEGER DEFAULT 0,    -- Wiedergabeposition (Hörspiele)
  last_played INTEGER
);

-- Equalizer-Kaskade: Track ▸ Album ▸ Interpret ▸ Global
CREATE TABLE eq_preset (
  id INTEGER PRIMARY KEY,
  preamp REAL DEFAULT 0,
  bands  TEXT NOT NULL            -- JSON [g0..g9] in dB (-24..+12)
);
CREATE TABLE eq_binding (
  scope     TEXT CHECK(scope IN ('global','artist','album','track')),
  target_id INTEGER,              -- NULL bei global
  preset_id INTEGER REFERENCES eq_preset(id),
  PRIMARY KEY(scope, target_id)
);
```

## Equalizer-Konzept

- GStreamer `equalizer-10bands` als `audio-filter` in `playbin3`; Band-Gains
  werden beim Tracklauf **live** gesetzt (kein Stream-Neustart).
- Auflösung beim Abspielen: spezifischste vorhandene Bindung gewinnt
  (`track` → `album` → `artist` → `global`).
- Optional obendrauf: **Kopfhörer-/Ausgabe-Profile** (z. B. „In-Ear neutral",
  „BT-Box basslastig") als zusätzliche, manuell wählbare Ebene.

## Roadmap

### Phase 0 – Setup
- [x] Rust-Toolchain installieren (`rustup`), Ziel-Profil festlegen
- [x] Cargo-Projekt + `Cargo.toml` mit Crates
- [x] Kompilierende Adw-App: `NavigationSplitView`, leere Module
- [x] Flatpak-Manifest-Stub (optional, für späteres Packaging)

### Phase 1 – MVP
- [x] DB-Schema + Migrations (`rusqlite`)
- [x] Scanner: Ordner rekursiv, `lofty`-Metadaten → DB (Hintergrund-Worker)
- [x] Dateisystem-Browser (erste Navigation)
- [x] Playback via `playbin3`: Play/Pause/Next/Prev, Position-Slider
- [x] Mini-Player-Leiste
- [x] MPRIS-Anbindung (Lockscreen/Medientasten)

### Phase 2 – Bibliothek
- [x] Interpreten- & Album-Ansicht aus Metadaten
- [x] Cover-Cache (XDG-Cache)
- [x] Queue / Wiedergabeliste
- [x] **Resume-Position pro Track** speichern & anbieten

### Phase 3 – Equalizer
- [x] `equalizer-10bands` im Audio-Graph
- [x] EQ-Editor-UI (10 Bänder + Preamp)
- [x] Kaskaden-Auflösung global → Interpret → Album → Track
- [x] Kopfhörer-/Ausgabe-Profile

### Phase 4 – Erweiterungen
- [ ] Streaming-Backend (Subsonic/Navidrome oder Jellyfin)
- [ ] Podcasts (Feeds abonnieren, Episoden laden)

## Offene Entscheidungen

- App-ID / Namespace (z. B. `de.cais.Emilia`)?
- Streaming-Backend in Phase 4: Subsonic/Navidrome **oder** Jellyfin zuerst?
- Packaging-Weg final: Flatpak (Flathub) vs. Distro-Pakete (Mobian/postmarketOS)?
