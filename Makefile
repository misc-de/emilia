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
ICON_APP  = $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps
ICON_ACT  = $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/actions
LOCALE_DIR = $(DESTDIR)$(PREFIX)/share/locale

# Sprachen mit Katalog (Englisch ist Quellsprache, braucht keinen).
LINGUAS = $(shell grep -v '^\#' po/LINGUAS 2>/dev/null)
MO_FILES = $(patsubst %,po/%/LC_MESSAGES/emilia.mo,$(LINGUAS))

.PHONY: build mo install install-mo uninstall check pot run clean-mo

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
	install -Dm644 data/icons/hicolor/scalable/apps/$(APPID).svg $(ICON_APP)/$(APPID).svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-concert-symbolic.svg $(ICON_ACT)/emilia-concert-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/list-high-priority-symbolic.svg $(ICON_ACT)/list-high-priority-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-favorite-symbolic.svg $(ICON_ACT)/emilia-favorite-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-audiobook-symbolic.svg $(ICON_ACT)/emilia-audiobook-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-stats-symbolic.svg $(ICON_ACT)/emilia-stats-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-share-symbolic.svg $(ICON_ACT)/emilia-share-symbolic.svg
	@echo "Installiert nach $(PREFIX). Ggf. Icon-Cache/Desktop-DB aktualisieren:"
	@echo "  gtk4-update-icon-cache $(PREFIX)/share/icons/hicolor"
	@echo "  update-desktop-database $(PREFIX)/share/applications"

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
	rm -f $(ICON_APP)/$(APPID).svg
	rm -f $(ICON_ACT)/emilia-concert-symbolic.svg
	rm -f $(ICON_ACT)/emilia-share-symbolic.svg
	@for lang in $(LINGUAS); do \
		rm -f $(LOCALE_DIR)/$$lang/LC_MESSAGES/emilia.mo; \
	done

# Vorlage (.pot) aus den Quelltexten extrahieren (benötigt xgettext).
pot:
	xgettext --from-code=UTF-8 --language=C --keyword=gettext \
		--keyword=ngettext:1,2 --keyword=gettext_f --keyword=ngettext_n:1,2 \
		--add-comments=TRANSLATORS --files-from=po/POTFILES.in \
		--package-name=Emilia -o po/emilia.pot
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
