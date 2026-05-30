//! Erkennung möglicher Konzerte: Alben (Ordner), deren Name auf ein Konzert
//! hindeutet, sowie einzelne lange Audiodateien (ab 30 Minuten).

use std::collections::HashSet;
use std::path::Path;

use crate::core::scanner;

/// Schlüsselwörter, die ein „Konzert-Album" vermuten lassen.
const KEYWORDS: &[&str] = &["concert", "konzert", "live", "unplugged"];
/// Mindestlänge einer Einzeldatei, um als Konzert zu gelten.
const MIN_TRACK_SECS: u64 = 30 * 60;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: String,
    pub title: String,
    pub subtitle: String,
    pub is_dir: bool,
}

fn name_matches(name: &str) -> bool {
    let lower = name.to_lowercase();
    KEYWORDS.iter().any(|k| lower.contains(k))
}

fn count_audio(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter(|e| scanner::is_audio(&e.path())).count())
        .unwrap_or(0)
}

fn fmt_hms(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Durchsucht `root` nach Konzert-Kandidaten. Bereits markierte (`existing`)
/// werden ausgelassen. Läuft im Hintergrund-Thread (liest Dateidauern).
pub fn scan_candidates(root: &Path, existing: &HashSet<String>) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs = Vec::new();
        let mut files = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                subdirs.push(p);
            } else if scanner::is_audio(&p) {
                files.push(p);
            }
        }

        for sd in subdirs {
            let name = sd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name_matches(&name) {
                let count = count_audio(&sd);
                if count > 0 {
                    let path = sd.to_string_lossy().into_owned();
                    if !existing.contains(&path) {
                        out.push(Candidate {
                            path,
                            title: name,
                            subtitle: format!("Album · {count} Titel"),
                            is_dir: true,
                        });
                    }
                    // Ganzes Album = ein Konzert → nicht weiter absteigen.
                    continue;
                }
            }
            stack.push(sd);
        }

        // Einzelne lange Dateien (≥ 30 min).
        for f in files {
            let secs = scanner::duration_secs(&f);
            if secs >= MIN_TRACK_SECS {
                let path = f.to_string_lossy().into_owned();
                if existing.contains(&path) {
                    continue;
                }
                let title = f
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                out.push(Candidate {
                    path,
                    title,
                    subtitle: format!("Datei · {}", fmt_hms(secs)),
                    is_dir: false,
                });
            }
        }
    }

    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    out
}
