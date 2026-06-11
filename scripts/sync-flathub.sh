#!/usr/bin/env bash
# Hält das Flathub-Manifest mit dem echten Release-Stand synchron.
#
# Hintergrund: In diesem Repo gibt es ZWEI Versions-Begriffe, die immer wieder
# auseinanderlaufen:
#   * Cargo.toml  -> reiner Commit-Zähler, der pre-commit-Hook bumpt ihn bei
#                    JEDEM Commit. NICHT die Release-Version.
#   * Release     -> die Marketing-Version, die du von Hand setzt. Sie lebt an
#                    drei Stellen, die zusammenpassen MÜSSEN:
#                       1. oberster <release version="X"> in der MetaInfo
#                       2. Git-Tag  vX
#                       3. tag:/commit: in de.cais.Emilia.flathub.yaml
#
# Die MetaInfo + der Git-Tag sind die Quelle der Wahrheit; dieses Script zieht
# das Flathub-Manifest darauf nach (oder prüft im --check-Modus nur).
#
# Nutzung:
#   scripts/sync-flathub.sh          # flathub.yaml auf MetaInfo-Top-Release ziehen
#   scripts/sync-flathub.sh --check  # nur prüfen, Exit 1 bei Drift (für CI/Hook)
#
# Read-only gegenüber Git (nur rev-parse/tag -l). Erzeugt KEINE Tags und
# committet nichts – Tags/Releases bleiben bewusst in deiner Hand.

set -euo pipefail

root="$(git rev-parse --show-toplevel)"
metainfo="$root/data/de.cais.Emilia.metainfo.xml"
manifest="$root/de.cais.Emilia.flathub.yaml"

check_only=0
[ "${1:-}" = "--check" ] && check_only=1

for f in "$metainfo" "$manifest"; do
    [ -f "$f" ] || { echo "sync-flathub: $f fehlt." >&2; exit 2; }
done

# Oberster <release version="X" …> aus der MetaInfo = Release-Version.
version="$(sed -n -E 's/.*<release[[:space:]]+version="([0-9]+\.[0-9]+\.[0-9]+)".*/\1/p' "$metainfo" | head -1)"
[ -n "$version" ] || { echo "sync-flathub: kein <release version=…> in der MetaInfo gefunden." >&2; exit 2; }
tag="v$version"

# Git-Tag muss existieren (Release-Commit). Wir erzeugen ihn NICHT automatisch.
if ! sha="$(git rev-parse -q --verify "refs/tags/$tag^{commit}" 2>/dev/null)"; then
    echo "sync-flathub: Git-Tag '$tag' existiert nicht." >&2
    echo "  Lege ihn auf den Release-Commit:  git tag $tag <commit> && git push origin $tag" >&2
    exit 2
fi

# Ist-Werte aus dem Manifest.
cur_tag="$(sed -n -E 's/^[[:space:]]*tag:[[:space:]]*(\S+).*/\1/p' "$manifest" | head -1)"
cur_commit="$(sed -n -E 's/^[[:space:]]*commit:[[:space:]]*([0-9a-f]+).*/\1/p' "$manifest" | head -1)"

if [ "$cur_tag" = "$tag" ] && [ "$cur_commit" = "$sha" ]; then
    [ "$check_only" -eq 1 ] && exit 0
    echo "sync-flathub: bereits synchron ($tag @ ${sha:0:10})."
    exit 0
fi

if [ "$check_only" -eq 1 ]; then
    echo "sync-flathub: DRIFT – Flathub-Manifest passt nicht zum Release." >&2
    echo "  MetaInfo-Top-Release : $tag  ($sha)" >&2
    echo "  flathub.yaml tag     : ${cur_tag:-<leer>}" >&2
    echo "  flathub.yaml commit  : ${cur_commit:-<leer>}" >&2
    echo "  Beheben:  scripts/sync-flathub.sh" >&2
    exit 1
fi

# Schreiben: tag:, commit: und die Kommentarzeile angleichen.
sed -i -E \
    -e "s|^([[:space:]]*tag:[[:space:]]*).*|\1$tag|" \
    -e "s|^([[:space:]]*commit:[[:space:]]*).*|\1$sha|" \
    -e "s|^([[:space:]]*# Release )[0-9]+\.[0-9]+\.[0-9]+( == MetaInfo-Top-Release.*)|\1$version\2|" \
    "$manifest"

echo "sync-flathub: aktualisiert -> $tag @ ${sha:0:10}"
echo "  (denk daran, das Manifest mit zu committen)"
