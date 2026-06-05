# Flathub-Einreichung

Diese Datei beschreibt den Weg von Emilia auf [Flathub](https://flathub.org).
Der **eigene** signierte OSTree-Repo-Release (GitHub Pages) bleibt davon
unberührt und läuft weiter über `make flatpak-build` mit `de.cais.Emilia.yaml`.

## Zwei Manifeste – warum?

| Datei | Quelle | Zweck |
|-------|--------|-------|
| `de.cais.Emilia.yaml` | `type: dir` (Arbeitsverzeichnis) | Lokale Builds + eigener GitHub-Pages-Repo-Release |
| `de.cais.Emilia.flathub.yaml` | `type: git` (fester Tag) | Flathub – baut nur aus dem gepushten, getaggten Stand |

Beide nutzen dasselbe `generated-sources.json` (Offline-Vendoring der Crates).

## Status der Flathub-Voraussetzungen

- [x] Reverse-DNS-App-ID, MetaInfo/Desktop/Icon valide, OARS, GPL-3.0-or-later
- [x] Screenshots im `<screenshots>`-Block (`data/screenshots/`, Raw-URLs auf `main`)
- [x] Offline-Build via `generated-sources.json` (verifiziert: baut ohne Netz)
- [ ] **App-Quelle auf `type: git` + Tag** → `de.cais.Emilia.flathub.yaml` (Tag muss noch gesetzt werden)

## Schritte für die Einreichung

1. **Alles committen**, was ins Release gehört – insbesondere:
   - `generated-sources.json`
   - `data/screenshots/desktop_artists.png`, `data/screenshots/mobile_artists.png`
   - `data/de.cais.Emilia.metainfo.xml` (mit `<screenshots>`)
   - `de.cais.Emilia.flathub.yaml`

   > Hinweis: Der pre-commit-Hook erhöht bei jedem Commit die Patch-Version in
   > `Cargo.toml`. Die Tag-Version (Schritt 3) muss zur **finalen** Version nach
   > dem letzten Commit passen. Idealerweise auch ein `<release>` mit dieser
   > Version oben in der MetaInfo ergänzen (aktuell jüngstes: 0.1.45).

2. **Pushen** nach `main` (sonst sind die Screenshot-Raw-URLs und der Tag für
   Flathub nicht erreichbar):

   ```sh
   git push origin main
   ```

3. **Tag setzen und pushen** (Version an die finale `Cargo.toml` anpassen):

   ```sh
   git tag v0.1.49        # = Version aus Cargo.toml
   git push origin v0.1.49
   ```

   Dann in `de.cais.Emilia.flathub.yaml` `tag:` auf denselben Wert setzen und
   optional `commit:` mit dem SHA (`git rev-parse v0.1.49`) ergänzen.

4. **Lokal gegen den getaggten Stand testen** (baut jetzt wirklich aus Git):

   ```sh
   flatpak-builder --user --install --force-clean build-dir \
       de.cais.Emilia.flathub.yaml
   flatpak run de.cais.Emilia
   ```

5. **Flathub-Linter** laufen lassen (das prüft Flathub im CI):

   ```sh
   flatpak install flathub org.flatpak.Builder
   flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
       manifest de.cais.Emilia.flathub.yaml
   flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
       repo repo            # nach einem Build mit --repo=repo
   ```

6. **Einreichen**: Fork von `flathub/flathub`, neuer Branch `de.cais.Emilia`,
   darin `de.cais.Emilia.yaml` (= diese flathub-Variante, umbenannt) **und**
   `generated-sources.json` ablegen, dann Pull Request öffnen. Details:
   <https://docs.flathub.org/docs/for-app-authors/submission>

## Reviewer-Fragen — vorab beantwortet

Legende: ✅ begründet/unkritisch · ⚠️ Reviewer hakt nach, Antwort steht ·
❌ echter Handlungsbedarf vor Einreichung (siehe letzter Abschnitt).

### Laufzeit-Tools in `org.gnome.Platform//49` (gegen das installierte Runtime verifiziert)

Die App ruft mehrere externe Programme/GStreamer-Plugins auf. Was vorhanden ist
und was nicht:

| Extern aufgerufen | Im Runtime? | Feature | Folge bei Fehlen |
|---|---|---|---|
| `ffmpeg` | ✅ | YouTube-Offline → MP3-Transcode | — |
| `pactl` | ✅ | Audio-Ausgabegeräte auflisten | — |
| `secret-tool` (libsecret) | ✅ | Keyring statt Klartext-DB | — |
| `python3` | ✅ | führt den `yt-dlp`-Zipapp aus | — |
| `gtk4paintablesink` | ✅ | Live-Kameravorschau | Vorschau aus (Code degradiert sauber) |
| `v4l2src`/`autovideosrc` | ✅ | Kamera-Quelle (QR-Scan) | — (Halium: via `droidcam2v4l2`) |
| ~~`zxing`~~ (nicht mehr nötig) | — | QR-Dekodierung jetzt **in-process** (`rqrr`) | — |
| `fpcalc` (Chromaprint) | ❌ **fehlt** | AcoustID-Fingerprint | Fingerprint-Erkennung aus |

### finish-args — Begründung je Berechtigung

- `--share=ipc`, `--socket=wayland`, `--socket=fallback-x11`, `--device=dri` —
  Standard-GUI/Rendering. ✅
- `--socket=pulseaudio` — Audiowiedergabe (PipeWire über Pulse-Kompat). ✅
- `--share=network` — Online-Metadaten (MusicBrainz/Deezer/Cover Art Archive/
  iTunes), Podcasts/Radiostreams **und** LAN-Geräte-Sync (HTTPS-Server+Client).
  Der Sync-Server lauscht nur, solange die App offen ist — **kein**
  Hintergrunddienst, **kein** Autostart. ✅ begründbar
- `--filesystem=xdg-music` — eng auf den Musikordner begrenzt; weitere Ordner
  wählt der Nutzer über den **Portal**-Dateidialog (ohne Zusatzberechtigung).
  Schreibzugriff nur für Stream-Aufnahmen + per Sync empfangene Dateien. ✅ eng
- `--own-name=org.mpris.MediaPlayer2.Emilia.*` — MPRIS (Sperrbildschirm/
  Medientasten), Suffix pro Instanz. ✅
- `--talk-name=org.freedesktop.secrets` — libsecret/Keyring für Nextcloud-Zugang
  + API-Keys; `secret-tool` ist im Runtime vorhanden, der Keyring greift also
  wirklich (keine Klartext-Secrets im Flatpak). ✅
- `--device=all` — ⚠️ Beantragt für die Kamera (QR-Pairing). Der QR-Scan
  **funktioniert jetzt** (Dekodierung in-process via `rqrr`, Quelle
  `v4l2src`/`autovideosrc` aus der Runtime; auf Halium/FuriOS/Droidian ist die
  Kamera über `droidcam2v4l2` als `/dev/video*` erreichbar). Der Reviewer
  bevorzugt zwar das **Camera-Portal** (PipeWire) statt rohem `--device=all`,
  aber das Portal exponiert die V4L2-Loopbacks der Halium-Brücke nicht
  zuverlässig — daher der direkte Geräte-Zugriff. `--device=dri` ist redundant
  (in `all` enthalten) und kann entfallen.

### App-Metadaten / Identität

- App-ID `de.cais.Emilia`, Code auf `github.com/misc-de` → der Reviewer verlangt
  einen **Nachweis der Kontrolle über `cais.de`** (Homepage-URL zeigt bereits
  dorthin; ggf. genügt ein Link von cais.de aufs Repo oder umgekehrt). ⚠️
- `metadata_license: CC0-1.0`, `project_license: GPL-3.0-or-later`, OARS-
  `content_rating`, Screenshots, `<developer>`, Release-Notes — alle vorhanden. ✅
  **Noch ergänzen:** ein `<release>`-Eintrag für die getaggte Version (v0.1.49)
  ganz oben in der MetaInfo (aktuell jüngstes: 0.1.45).
- Alte Release-Note 0.1.4 nennt „in-app self-update / Selbst-Aktualisierung".
  Im Code gibt es **keinen** Binary-Updater — gemeint ist der App-Neustart nach
  Sprachwechsel (`current_exe().spawn()` in `src/ui/app.rs`). Formulierung
  entschärfen, damit der Reviewer beim Lesen der MetaInfo nicht stutzt. ⚠️

### Build

- Offline-Build (`cargo build --offline`, vendored Crates via
  `generated-sources.json`), kein Netz beim Bauen. ✅
- App-Quelle `type: git` + fester Tag/Commit. ✅ (Tag noch setzen, s. o.)

### `yt-dlp` — gebündelt + verifiziert ✅ (gelöst)

Früher lud die App `yt-dlp` zur Laufzeit **ungeprüft** (latest, ohne Prüfsumme)
von GitHub — genau das spricht ein Reviewer an. Jetzt:

- **Mitgeliefert + sha256-verifiziert:** `yt-dlp`-Modul im Manifest pinnt eine
  konkrete Release (`type: file` + `sha256`) und installiert sie nach
  `/app/bin/yt-dlp`. Kein ungeprüftes Nachladen mehr; der ausgelieferte Stand ist
  reproduzierbar und Teil des Builds.
- **Bleibt automatisch aktuell:** `x-checker-data` (json gegen die GitHub-
  Releases-API) lässt Flathubs `flatpak-external-data-checker` neue yt-dlp-
  Versionen erkennen und **selbständig Update-PRs** öffnen — kein manuelles
  Nachziehen des Pins bei jedem Commit.
- **Fallback für Aktualität zwischen Releases:** Der In-App-„yt-dlp
  aktualisieren"-Knopf lädt bei Bedarf eine neuere Version in den Datenordner;
  diese Kopie hat Vorrang vor der gebündelten Baseline (`src/core/youtube.rs`).
  Außerdem frischt die App nach einem Emilia-Update beim Start (online) yt-dlp auf.
- **Reststatus:** Der optionale Nutzer-Download (Fallback) ist weiterhin ein
  HTTPS-Transfer ohne GPG-Pin; das Supply-Chain-Restrisiko betrifft aber nur noch
  diesen freiwilligen Schritt, nicht den ausgelieferten Standard. Ausführung
  ohnehin nur in der Sandbox mit App-Rechten.

## Vor Einreichung zu klärende Punkte (echter Handlungsbedarf)

1. **Kamera/QR** ✅ **gelöst** — QR-Dekodierung läuft jetzt in-process (`rqrr`),
   kein `zxing`-Plugin mehr nötig; Kamera-Quelle `v4l2src`/`autovideosrc` aus der
   Runtime, auf Halium/FuriOS/Droidian über `droidcam2v4l2` (`/dev/video*`).
   `EMILIA_CAMERA_SRC` erlaubt Port-Overrides. Verbleibt nur die Reviewer-
   Diskussion `--device=all` vs. Camera-Portal (Begründung oben); `--device=dri`
   kann als redundant entfallen.
2. **`yt-dlp` zur Laufzeit** ✅ **gelöst** — jetzt gebündelt + sha256-verifiziert
   im Manifest, automatisch aktuell via `x-checker-data`, optionaler Nutzer-Update
   als Fallback (Vorrang vor Baseline). Details oben.
3. **`fpcalc` fehlt** ⚠️ — AcoustID-Fingerprint funktioniert im Flatpak nicht.
   Optional Chromaprint/`fpcalc` als Modul bündeln, sonst Feature bewusst
   deaktiviert lassen (degradiert bereits sauber).

## generated-sources.json neu erzeugen (nach Cargo.lock-Änderung)

```sh
.flathub-tools/venv/bin/python .flathub-tools/flatpak-cargo-generator.py \
    Cargo.lock -o generated-sources.json
```
