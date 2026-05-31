# Installation von Emilia (Binary, .desktop, Icons, AppStream-Metainfo).
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

.PHONY: build install uninstall check

build:
	cargo build --release

install: build
	install -Dm755 target/release/emilia $(BIN_DIR)/emilia
	install -Dm644 data/$(APPID).desktop $(APP_DIR)/$(APPID).desktop
	install -Dm644 data/$(APPID).metainfo.xml $(META_DIR)/$(APPID).metainfo.xml
	install -Dm644 data/icons/hicolor/scalable/apps/$(APPID).svg $(ICON_APP)/$(APPID).svg
	install -Dm644 data/icons/hicolor/scalable/actions/emilia-concert-symbolic.svg $(ICON_ACT)/emilia-concert-symbolic.svg
	install -Dm644 data/icons/hicolor/scalable/actions/list-high-priority-symbolic.svg $(ICON_ACT)/list-high-priority-symbolic.svg
	@echo "Installiert nach $(PREFIX). Ggf. Icon-Cache/Desktop-DB aktualisieren:"
	@echo "  gtk4-update-icon-cache $(PREFIX)/share/icons/hicolor"
	@echo "  update-desktop-database $(PREFIX)/share/applications"

uninstall:
	rm -f $(BIN_DIR)/emilia
	rm -f $(APP_DIR)/$(APPID).desktop
	rm -f $(META_DIR)/$(APPID).metainfo.xml
	rm -f $(ICON_APP)/$(APPID).svg
	rm -f $(ICON_ACT)/emilia-concert-symbolic.svg

# Validiert die Metadaten-Dateien (sofern die Werkzeuge vorhanden sind).
check:
	-desktop-file-validate data/$(APPID).desktop
	-appstreamcli validate --no-net data/$(APPID).metainfo.xml
