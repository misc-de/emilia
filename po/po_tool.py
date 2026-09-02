#!/usr/bin/env python3
# Übersetzungshelfer für die Kataloge unter po/ (braucht `polib`, z. B. aus
# .flathub-tools/venv). Ablauf nach `make pot` + `msgmerge -U po/<lang>.po po/emilia.pot`:
#
#   python3 po/po_tool.py dump  <lang> /tmp/<lang>_open.json   # offene Einträge exportieren
#   … Übersetzungen als JSON-Liste schreiben (Format siehe unten) …
#   python3 po/po_tool.py apply <lang> /tmp/<lang>_done.json   # anwenden + validieren
#
# `dump` liefert je offenem Eintrag msgid, Kontext, msgmerge-Fuzzy-Vorschlag,
# die deutsche Übersetzung als zweite Referenz, Fundstellen und Pluralformen.
# `apply` prüft Platzhalter ({n}, {title} …), Pluralanzahl und `msgfmt --check`
# und lehnt fehlerhafte Einträge einzeln ab. Die .po-Dateien sind die Quelle
# der Wahrheit; die alten lang_*.py-Wörterbücher in .flathub-tools sind Historie.
"""Emilia translation helper (run from the repo root with the venv python).

  po_tool.py dump  <lang> <out.json>   # open (untranslated + fuzzy) entries, with the
                                       # German translation as a second reference
  po_tool.py apply <lang> <in.json>    # write translations back, validate, msgfmt --check

JSON format for apply: a list of objects {"key": <key from dump>, "msgstr": "..."}
for singular entries, or {"key": ..., "msgstr_plural": ["form0", "form1", ...]} for
plural entries (exactly nplurals forms, taken from the catalog header).
"""
import json, re, subprocess, sys
import polib

PH = re.compile(r"\{[a-zA-Z_][a-zA-Z0-9_]*\}")

def key_of(e):
    return (e.msgctxt or "") + "\x04" + e.msgid

def nplurals_of(po):
    pf = po.metadata.get("Plural-Forms", "")
    m = re.search(r"nplurals\s*=\s*(\d+)", pf)
    return int(m.group(1)) if m else 2

def dump(lang, out):
    po = polib.pofile(f"po/{lang}.po")
    de = {key_of(e): e for e in polib.pofile("po/de.po") if not e.obsolete}
    items, style = [], []
    for e in po:
        if e.obsolete:
            continue
        k = key_of(e)
        d = de.get(k)
        if (not e.translated()) or ("fuzzy" in e.flags):
            it = {
                "key": k,
                "msgid": e.msgid,
                "msgctxt": e.msgctxt,
                "fuzzy_suggestion": (e.msgstr or None) if "fuzzy" in e.flags else None,
                "german": (d.msgstr if d and not d.msgid_plural else None),
                "where": [o[0] for o in e.occurrences][:3],
                "comment": e.comment or None,
            }
            if e.msgid_plural:
                it["msgid_plural"] = e.msgid_plural
                it["german_plural"] = [d.msgstr_plural[i] for i in sorted(d.msgstr_plural)] if d else None
                if "fuzzy" in e.flags:
                    it["fuzzy_suggestion"] = [e.msgstr_plural[i] for i in sorted(e.msgstr_plural)]
            items.append(it)
        elif len(style) < 60 and e.msgstr and not e.msgid_plural:
            style.append({"msgid": e.msgid, "msgstr": e.msgstr})
    data = {
        "lang": lang,
        "nplurals": nplurals_of(po),
        "plural_forms": po.metadata.get("Plural-Forms", ""),
        "open_count": len(items),
        "style_reference": style,
        "entries": items,
    }
    json.dump(data, open(out, "w"), ensure_ascii=False, indent=1)
    print(f"[{lang}] {len(items)} open entries → {out} (nplurals={data['nplurals']})")

def apply(lang, inp):
    po = polib.pofile(f"po/{lang}.po")
    npl = nplurals_of(po)
    raw = json.load(open(inp))
    trans = {t["key"]: t for t in (raw["entries"] if isinstance(raw, dict) else raw)}
    applied, problems = 0, []
    for e in po:
        if e.obsolete:
            continue
        t = trans.get(key_of(e))
        if not t:
            continue
        if e.msgid_plural:
            forms = t.get("msgstr_plural")
            if not forms or len(forms) != npl or any(not f.strip() for f in forms):
                problems.append(f"PLURAL forms != {npl} or empty: {e.msgid!r}")
                continue
            src = set(PH.findall(e.msgid)) | set(PH.findall(e.msgid_plural))
            bad = [f for f in forms if set(PH.findall(f)) != src]
            if bad:
                problems.append(f"PLACEHOLDER {e.msgid!r}: {src} vs {bad}")
                continue
            e.msgstr_plural = {i: forms[i] for i in range(npl)}
        else:
            s = t.get("msgstr")
            if not s or not s.strip():
                problems.append(f"EMPTY: {e.msgid!r}")
                continue
            if set(PH.findall(e.msgid)) != set(PH.findall(s)):
                problems.append(f"PLACEHOLDER {e.msgid!r}: {set(PH.findall(e.msgid))} vs {set(PH.findall(s))}")
                continue
            if e.msgid.endswith("…") != s.endswith("…") and "…" in e.msgid:
                problems.append(f"ELLIPSIS lost: {e.msgid!r} → {s!r}")
            e.msgstr = s
        if "fuzzy" in e.flags:
            e.flags.remove("fuzzy")
        applied += 1
    po.save(f"po/{lang}.po")
    remaining = [e.msgid for e in po if not e.obsolete and ((not e.translated()) or "fuzzy" in e.flags)]
    print(f"[{lang}] applied {applied}, problems {len(problems)}, still open {len(remaining)}")
    for p in problems[:40]:
        print("  ", p)
    for m in remaining[:20]:
        print("   OPEN:", repr(m)[:100])
    r = subprocess.run(["msgfmt", "--check", "--statistics", f"po/{lang}.po", "-o", "/dev/null"], capture_output=True, text=True)
    print("  msgfmt:", (r.stderr or r.stdout).strip(), "| exit", r.returncode)
    return 0 if (not problems and not remaining and r.returncode == 0) else 1

if __name__ == "__main__":
    cmd, lang, path = sys.argv[1:4]
    sys.exit(dump(lang, path) if cmd == "dump" else apply(lang, path))
