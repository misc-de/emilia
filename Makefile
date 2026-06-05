# Installation von Emilia (Binary, .desktop, Icons, AppStream-Metainfo,
# Übersetzungen).
#
# Systemweit:   sudo make install
# Benutzer:     make install PREFIX=$HOME/.local
# Paketbau:     make install DESTDIR=/pfad/zum/staging PREFIX=/usr

PREFIX  ?= /usr/local
DESTDIR ?=
APPID    = de.cais.Emilia

BIN_DIR   = $(DESTDIR)$(PREFIX)/bin
APP_DIR   = $(DESTDIR)$(PREFIX)/share/applications
META_DIR  = $(DESTDIR)$(PREFIX)/share/metainfo
ICON_APP  = $(DESTDIR)$(PREFIX)/share/icons/hicolor/256x256/apps
ICON_ACT  = $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/actions
LOCALE_DIR = $(DESTDIR)$(PREFIX)/share/locale

# Sprachen mit Katalog (Englisch ist Quellsprache, braucht keinen).
LINGUAS = $(shell grep -v '^\#' po/LINGUAS 2>/dev/null)
MO_FILES = $(patsubst %,po/%/LC_MESSAGES/emilia.mo,$(LINGUAS))

.PHONY: build mo install install-mo uninstall check pot run clean-mo release

build:
	cargo build --release

# Übersetzungskataloge (.po → .mo) bauen.
mo: $(MO_FILES)

po/%/LC_MESSAGES/emilia.mo: po/%.po
	mkdir -p $(dir $@)
	msgfmt --check $< -o $@

install: build mo install-mo
	install -Dm755 target/release/emilia $(BIN_DIR)/emilia
	install -Dm644 data/$(APPID).desktop $(APP_DIR)/$(APPID).desktop
	install -Dm644 data/$(APPID).metainfo.xml $(META_DIR)/$(APPID).metainfo.xml
	install -Dm644 data/icons/hicolor/256x256/apps/$(APPID).png $(ICON_APP)/$(APPID).png
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-concert-symbolic.svg $(ICON_ACT)/emilia-concert-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/list-high-priority-symbolic.svg $(ICON_ACT)/list-high-priority-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-favorite-symbolic.svg $(ICON_ACT)/emilia-favorite-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-audiobook-symbolic.svg $(ICON_ACT)/emilia-audiobook-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-stats-symbolic.svg $(ICON_ACT)/emilia-stats-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-share-symbolic.svg $(ICON_ACT)/emilia-share-symbolic.svg
	@# Icon-Cache & Desktop-DB auffrischen, damit das neue App-Icon sofort
	@# erscheint (nur bei direkter Installation, nicht in eine DESTDIR-Stage).
	@if [ -z "$(DESTDIR)" ]; then \
	  gtk4-update-icon-cache -f -t "$(PREFIX)/share/icons/hicolor" 2>/dev/null || true; \
	  update-desktop-database "$(PREFIX)/share/applications" 2>/dev/null || true; \
	fi
	@echo "Installiert nach $(PREFIX)."

# Kataloge nach <prefix>/share/locale/<lang>/LC_MESSAGES/emilia.mo legen.
install-mo: mo
	@for lang in $(LINGUAS); do \
		install -Dm644 po/$$lang/LC_MESSAGES/emilia.mo \
			$(LOCALE_DIR)/$$lang/LC_MESSAGES/emilia.mo; \
	done

uninstall:
	rm -f $(BIN_DIR)/emilia
	rm -f $(APP_DIR)/$(APPID).desktop
	rm -f $(META_DIR)/$(APPID).metainfo.xml
	rm -f $(ICON_APP)/$(APPID).png
	rm -f $(ICON_ACT)/emilia-concert-symbolic.svg
	rm -f $(ICON_ACT)/emilia-share-symbolic.svg
	@for lang in $(LINGUAS); do \
		rm -f $(LOCALE_DIR)/$$lang/LC_MESSAGES/emilia.mo; \
	done

# Vorlage (.pot) aus den Quelltexten extrahieren (benötigt xgettext).
# Strings extrahieren mit `xtr` (Rust-nativer gettext-Extraktor; `cargo install xtr`).
# xtr folgt der crate-root (src/main.rs) durch alle `mod` – im Gegensatz zu
# xgettext braucht es daher KEINE POTFILES.in und verschluckt keine Strings an
# Rust-Apostrophen (xgettext --language=C interpretierte ' als Zeichenkonstante
# und ließ ganze Dateien wie app_youtube.rs aus). --keywords ERSETZT die
# Default-Keywords, deshalb müssen gettext/ngettext hier mit aufgeführt werden.
pot:
	xtr --keywords=gettext --keywords=ngettext:1,2 --keywords=gettext_f \
		--keywords=ngettext_n:1,2 --package-name=Emilia \
		-o po/emilia.pot src/main.rs
	@echo "po/emilia.pot aktualisiert. Kataloge angleichen: msgmerge -U po/de.po po/emilia.pot"

# Entwicklung: Kataloge bauen und mit lokalem Katalogpfad starten.
# Sprache wählen: make run LANG_OVERRIDE=de  (oder en)
run: mo
	EMILIA_LOCALEDIR=$(PWD)/po LANGUAGE=$(LANG_OVERRIDE) cargo run

clean-mo:
	rm -rf $(addsuffix /LC_MESSAGES,$(addprefix po/,$(LINGUAS)))

# Validiert die Metadaten-Dateien (sofern die Werkzeuge vorhanden sind).
check:
	-desktop-file-validate data/$(APPID).desktop
	-appstreamcli validate --no-net data/$(APPID).metainfo.xml
	-msgfmt --check po/de.po -o /dev/null

# ---------------------------------------------------------------------------
# Release schneiden:  make release VERSION=0.2.0
#
# Setzt VERSION konsistent in Cargo.toml + Cargo.lock, verlangt einen passenden
# <release>-Eintrag in der Metainfo (die Notizen schreibst du vorher von Hand und
# committest sie), pinnt den Tag im Flathub-Manifest und taggt den Stand. Der
# Commit läuft mit --no-verify, damit der version-bump-Hook die gesetzte Version
# NICHT wieder hochzählt (sonst driftet Cargo.toml weg). Danach wird der Tag-SHA
# ins `commit:`-Feld des Flathub-Manifests gepinnt (von Flathub empfohlen).
# Hinweis: VERSION wird wörtlich übernommen – die Patch-Stelle ist ansonsten nur
# der Auto-Build-Zähler des Hooks, hier gibst du die echte Release-Version an.
# ---------------------------------------------------------------------------
release:
	@echo "$(VERSION)" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$$' || { echo "VERSION=x.y.z angeben, z. B. make release VERSION=0.2.0"; exit 1; }
	@git diff-index --quiet HEAD -- || { echo "Arbeitsbaum nicht sauber – erst committen oder stashen."; exit 1; }
	@! git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null || { echo "Tag v$(VERSION) existiert bereits."; exit 1; }
	@grep -q 'release version="$(VERSION)"' data/$(APPID).metainfo.xml || { echo "Kein <release version=\"$(VERSION)\"> in data/$(APPID).metainfo.xml – bitte zuerst die Release-Notiz ergänzen und committen."; exit 1; }
	sed -i -E 's/^version = "[0-9]+\.[0-9]+\.[0-9]+"/version = "$(VERSION)"/' Cargo.toml
	sed -i -E '/^name = "emilia"$$/{n;s/^version = "[0-9]+\.[0-9]+\.[0-9]+"/version = "$(VERSION)"/;}' Cargo.lock
	sed -i -E 's|^( *tag: )v[0-9]+\.[0-9]+\.[0-9]+|\1v$(VERSION)|' $(APPID).flathub.yaml
	git add Cargo.toml Cargo.lock $(APPID).flathub.yaml
	git commit --no-verify -m "Release: $(VERSION)"
	git tag "v$(VERSION)"
	sed -i -E 's|^( *)#? *commit:.*|\1commit: '"$$(git rev-parse v$(VERSION))"'|' $(APPID).flathub.yaml
	git add $(APPID).flathub.yaml
	git commit --no-verify -m "Release: Flathub-commit für v$(VERSION) pinnen"
	@echo "✓ v$(VERSION) getaggt. Pushen:  git push && git push origin v$(VERSION)"

# ---------------------------------------------------------------------------
# Flatpak: ein OSTree-Repo als Update-Quelle, das BEIDE Architekturen enthält.
#
# Jede Architektur wird nativ gebaut und ins Repo committet:
#   x86_64 :  make flatpak-build                       (auf diesem Rechner)
#   aarch64:  auf furios `make flatpak-build`, das dort entstandene repo/
#             zurueckkopieren, dann hier `make flatpak-merge ARM_REPO=repo-arm`
# Danach einmal `make flatpak-publish` (Summary/AppStream/Deltas, optional
# signiert) und das Verzeichnis $(FP_REPO) per HTTPS hosten.
# ---------------------------------------------------------------------------
FP_MANIFEST ?= $(APPID).yaml
FP_REPO     ?= repo
FP_ARCH      = $(shell flatpak --default-arch)
FP_BUILDDIR ?= .flatpak-build/$(FP_ARCH)
FP_GPG      ?=
FP_GPGHOME  ?=
FP_GPGARGS   = $(if $(FP_GPG),--gpg-sign=$(FP_GPG) $(if $(FP_GPGHOME),--gpg-homedir=$(FP_GPGHOME)),)
# flatpak-builder als Host-Tool, sonst die geflatpakte Variante org.flatpak.Builder.
FP_BUILDER   = $(shell command -v flatpak-builder >/dev/null 2>&1 \
		&& echo flatpak-builder || echo flatpak run org.flatpak.Builder)

.PHONY: flatpak-build flatpak-merge flatpak-publish flatpak-repo-info

# Baut die aktuelle Host-Architektur in $(FP_REPO).
flatpak-build:
	$(FP_BUILDER) --force-clean --repo=$(FP_REPO) $(FP_GPGARGS) \
		$(FP_BUILDDIR) $(FP_MANIFEST)
	@echo "$(FP_ARCH) liegt jetzt in $(FP_REPO)/. Refs: make flatpak-repo-info"

# Fuehrt ein auf anderer Architektur gebautes Repo (ARM_REPO=<pfad>) zusammen.
flatpak-merge:
	@test -n "$(ARM_REPO)" || { echo "ARM_REPO=<pfad> angeben (das von furios kopierte repo/)"; exit 1; }
	ostree --repo=$(FP_REPO) pull-local $(ARM_REPO)
	@echo "Zusammengefuehrt. Jetzt: make flatpak-publish"

# Schreibt Summary/AppStream/Statische-Deltas (signiert, falls FP_GPG gesetzt).
flatpak-publish:
	flatpak build-update-repo --generate-static-deltas --prune $(FP_GPGARGS) $(FP_REPO)
	@echo "$(FP_REPO)/ ist fertig zum Hosten (per HTTPS ausliefern)."

# Zeigt, welche App-Refs (Architekturen) aktuell im Repo liegen.
flatpak-repo-info:
	@ostree --repo=$(FP_REPO) refs 2>/dev/null | grep -E "^app/" | sort \
		|| echo "(noch kein $(FP_REPO) gebaut)"
