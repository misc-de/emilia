#!/usr/bin/env python3
"""Release-Notiz einer Version aus der AppStream-Metainfo als Markdown ausgeben.

    scripts/release-notes.py 0.8.27 [data/de.cais.Emilia.metainfo.xml]

Gibt die englische Notiz (Quellsprache) aus und darunter, falls vorhanden, die
deutsche Fassung. Das Makefile-Ziel `github-release` nimmt die Ausgabe als Text
des GitHub-Releases, damit Metainfo und GitHub dieselbe Notiz tragen.
"""
import sys
import xml.etree.ElementTree as ET

XML_LANG = "{http://www.w3.org/XML/1998/namespace}lang"
DEFAULT_METAINFO = "data/de.cais.Emilia.metainfo.xml"


def text_of(el):
    """Gesamten Text eines Elements (inkl. Inline-Kindern) mit normiertem Whitespace."""
    return " ".join("".join(el.itertext()).split())


def blocks(desc, lang):
    """Absätze/Listen einer Sprache (None = Quellsprache) als Markdown-Blöcke."""
    out = []
    for el in desc:
        if el.tag == "p":
            if el.get(XML_LANG) == lang:
                out.append(text_of(el))
        elif el.tag in ("ul", "ol"):
            # AppStream übersetzt Listen je <li>, nicht am <ul>/<ol>.
            items = [li for li in el.findall("li") if li.get(XML_LANG) == lang]
            if not items:
                continue
            lines = []
            for i, li in enumerate(items, 1):
                bullet = "-" if el.tag == "ul" else f"{i}."
                lines.append(f"{bullet} {text_of(li)}")
            out.append("\n".join(lines))
    return out


def main(argv):
    if len(argv) < 2 or argv[1] in ("-h", "--help"):
        print(__doc__.strip(), file=sys.stderr)
        return 2
    version = argv[1]
    path = argv[2] if len(argv) > 2 else DEFAULT_METAINFO

    root = ET.parse(path).getroot()
    rel = root.find(f"./releases/release[@version='{version}']")
    if rel is None:
        print(f"Kein <release version=\"{version}\"> in {path}.", file=sys.stderr)
        return 1
    desc = rel.find("description")
    if desc is None:
        print(f"<release version=\"{version}\"> hat keine <description>.", file=sys.stderr)
        return 1

    en = blocks(desc, None)
    de = blocks(desc, "de")
    if not en:
        print(f"<release version=\"{version}\"> hat keinen englischen Text.", file=sys.stderr)
        return 1

    parts = ["\n\n".join(en)]
    if de:
        parts.append("### Deutsch\n\n" + "\n\n".join(de))
    print("\n\n".join(parts))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
