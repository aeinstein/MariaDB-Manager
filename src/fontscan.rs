//! Findet die installierten TrueType-/OpenType-Schriften (System und Benutzer).
//!
//! Gelesen wird je Datei nur der Kopf und die `name`-Tabelle (per Seek) – die Schriftordner sind
//! oft mehrere hundert MB groß. Aufgenommen wird je Familie der normale Schnitt („Regular“);
//! der Familienname (Name-ID 1) ist zugleich der GDI-Name, den `gdifont` an `CreateFontW` gibt.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Eine installierte Schrift: Familienname, Datei und Index der Schrift in der Datei (bei `.ttc`).
#[derive(Clone, Debug)]
pub struct FontEntry {
    pub name: String,
    pub path: PathBuf,
    pub index: u32,
}

/// Bevorzugte Standardschriften, falls in den Einstellungen keine gewählt ist.
const PREFERRED: [&str; 5] = ["Arial", "Liberation Sans", "Helvetica", "DejaVu Sans", "Noto Sans"];

/// Alle installierten Schriften, nach Namen sortiert (einmalig ermittelt).
pub fn installed() -> &'static [FontEntry] {
    static FONTS: OnceLock<Vec<FontEntry>> = OnceLock::new();
    FONTS.get_or_init(scan)
}

/// Schrift mit diesem Namen, sonst die erste vorhandene bevorzugte, sonst irgendeine.
pub fn find(name: &str) -> Option<&'static FontEntry> {
    let all = installed();
    let by = |n: &str| all.iter().find(|f| f.name.eq_ignore_ascii_case(n));
    (!name.is_empty()).then(|| by(name)).flatten().or_else(|| PREFERRED.iter().find_map(|n| by(n))).or(all.first())
}

fn font_dirs() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut v = Vec::new();
    if cfg!(windows) {
        v.push(env("WINDIR").unwrap_or_else(|| "C:\\Windows".into()).join("Fonts"));
        if let Some(l) = env("LOCALAPPDATA") {
            v.push(l.join("Microsoft\\Windows\\Fonts"));
        }
    } else if cfg!(target_os = "macos") {
        v.extend(["/System/Library/Fonts", "/Library/Fonts"].map(PathBuf::from));
        if let Some(h) = env("HOME") {
            v.push(h.join("Library/Fonts"));
        }
    } else {
        v.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
        if let Some(h) = env("HOME") {
            v.push(h.join(".local/share/fonts"));
            v.push(h.join(".fonts"));
        }
    }
    v
}

fn scan() -> Vec<FontEntry> {
    let mut files = Vec::new();
    for d in font_dirs() {
        collect(&d, 0, &mut files);
    }
    let mut out: Vec<FontEntry> = Vec::new();
    for path in files {
        for (index, name) in read_file(&path).unwrap_or_default() {
            if !out.iter().any(|f| f.name.eq_ignore_ascii_case(&name)) {
                out.push(FontEntry { name, path: path.clone(), index });
            }
        }
    }
    out.sort_by_key(|f| f.name.to_lowercase());
    out
}

fn collect(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth < 4 {
                collect(&p, depth + 1, out);
            }
        } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc")) {
            out.push(p);
        }
    }
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn read_at(f: &mut File, off: u64, len: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut b = vec![0; len];
    f.read_exact(&mut b).ok()?;
    Some(b)
}

/// Normale Schnitte einer Datei als (Index, Familienname).
fn read_file(path: &Path) -> Option<Vec<(u32, String)>> {
    let mut f = File::open(path).ok()?;
    let head = read_at(&mut f, 0, 12)?;
    let offsets: Vec<u32> = if &head[0..4] == b"ttcf" {
        let n = be32(&head, 8)?.min(64) as usize;
        let b = read_at(&mut f, 12, n * 4)?;
        (0..n).filter_map(|i| be32(&b, i * 4)).collect()
    } else {
        vec![0]
    };
    let mut out = Vec::new();
    for (i, off) in offsets.into_iter().enumerate() {
        if let Some(name) = read_face(&mut f, off as u64) {
            out.push((i as u32, name));
        }
    }
    Some(out)
}

/// Familienname einer Schrift ab `off`, falls normaler Schnitt und keine Symbolschrift.
fn read_face(f: &mut File, off: u64) -> Option<String> {
    let head = read_at(f, off, 12)?;
    let tag = &head[0..4];
    if tag != [0, 1, 0, 0] && tag != b"OTTO" && tag != b"true" {
        return None;
    }
    let n = be16(&head, 4)? as usize;
    let dir = read_at(f, off + 12, n * 16)?;
    let rec = (0..n).find(|i| &dir[i * 16..i * 16 + 4] == b"name")?;
    let (toff, tlen) = (be32(&dir, rec * 16 + 8)?, be32(&dir, rec * 16 + 12)?);
    if tlen > 1 << 20 {
        return None;
    }
    let t = read_at(f, toff as u64, tlen as usize)?;
    let (count, strings) = (be16(&t, 2)? as usize, be16(&t, 4)? as usize);
    // Beste Fundstelle je Name-ID: Windows-Unicode Englisch > Windows-Unicode > Mac Roman
    let mut best: [(u8, Option<String>); 3] = Default::default();
    for i in 0..count {
        let r = 6 + i * 12;
        let (plat, enc, lang, id) = (be16(&t, r)?, be16(&t, r + 2)?, be16(&t, r + 4)?, be16(&t, r + 6)?);
        let (len, so) = (be16(&t, r + 8)? as usize, be16(&t, r + 10)? as usize);
        if plat == 3 && enc == 0 {
            return None; // Symbolschrift (Wingdings, Marlett …)
        }
        if !(1..=2).contains(&id) {
            continue;
        }
        let Some(raw) = t.get(strings + so..strings + so + len) else { continue };
        let (rank, s) = match plat {
            3 if enc == 1 || enc == 10 => {
                let u: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                (if lang == 0x409 { 3 } else { 2 }, String::from_utf16_lossy(&u))
            }
            1 if enc == 0 => (1, raw.iter().map(|&b| b as char).collect()),
            _ => continue,
        };
        let slot = &mut best[id as usize];
        if rank > slot.0 {
            *slot = (rank, Some(s));
        }
    }
    let family = best[1].1.take()?.trim().to_string();
    let sub = best[2].1.take().unwrap_or_default();
    let regular = matches!(sub.trim().to_ascii_lowercase().as_str(), "regular" | "normal" | "book" | "roman" | "standard");
    (regular && !family.is_empty() && !family.starts_with('.')).then_some(family)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_findet_schriften() {
        let all = installed();
        eprintln!("{} Schriften, z. B. {:?}", all.len(), all.iter().take(10).map(|f| &f.name).collect::<Vec<_>>());
        // Auf Systemen ohne Schriften (z. B. minimale CI-Container) gibt es nichts zu prüfen
        if cfg!(windows) {
            assert!(find("Arial").is_some_and(|f| f.name == "Arial"));
            assert!(!all.iter().any(|f| f.name == "Wingdings"));
        }
    }
}
