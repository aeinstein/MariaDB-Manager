//! Gespeicherte Abfragen: `.sql`-Dateien im Konfigurationsordner, gegliedert wie bei
//! Navicat nach `queries/<Verbindung>/<Datenbank>/<Name>.sql`.

use std::path::PathBuf;
use std::time::SystemTime;

pub struct Saved {
    pub name: String,
    pub modified: String,
    pub size: u64,
}

/// Wurzelordner aller gespeicherten Abfragen.
pub fn root() -> Option<PathBuf> {
    crate::config::data_dir().map(|d| d.join("queries"))
}

/// Datei-/Ordnernamen ohne verbotene Zeichen.
pub fn sanitize(s: &str) -> String {
    let t: String = s
        .trim()
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    let t = t.trim_matches('.').to_string();
    if t.is_empty() {
        "_".into()
    } else {
        t
    }
}

fn dir(conn: &str, db: &str) -> Option<PathBuf> {
    root().map(|r| r.join(sanitize(conn)).join(sanitize(db)))
}

pub fn path(conn: &str, db: &str, name: &str) -> Option<PathBuf> {
    dir(conn, db).map(|d| d.join(format!("{}.sql", sanitize(name))))
}

/// Alle Abfragen einer Datenbank, nach Name sortiert.
pub fn list(conn: &str, db: &str) -> Vec<Saved> {
    let Some(d) = dir(conn, db) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(d) else { return Vec::new() };
    let mut v: Vec<Saved> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("sql")))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let name = e.path().file_stem()?.to_string_lossy().into_owned();
            Some(Saved { name, modified: md.modified().map(fmt_time).unwrap_or_default(), size: md.len() })
        })
        .collect();
    v.sort_by_key(|s| s.name.to_lowercase());
    v
}

pub fn load(conn: &str, db: &str, name: &str) -> Result<String, String> {
    let p = path(conn, db, name).ok_or("Kein Konfigurationsordner gefunden")?;
    let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(decode(&b))
}

pub fn save(conn: &str, db: &str, name: &str, sql: &str) -> Result<(), String> {
    let p = path(conn, db, name).ok_or("Kein Konfigurationsordner gefunden")?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, sql).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn exists(conn: &str, db: &str, name: &str) -> bool {
    path(conn, db, name).is_some_and(|p| p.exists())
}

pub fn delete(conn: &str, db: &str, name: &str) -> Result<(), String> {
    let p = path(conn, db, name).ok_or("Kein Konfigurationsordner gefunden")?;
    std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Verbindung umbenannt: Ordner mitnehmen (falls das Ziel noch nicht existiert).
pub fn rename_conn(old: &str, new: &str) {
    let (Some(r), true) = (root(), sanitize(old) != sanitize(new)) else { return };
    let (from, to) = (r.join(sanitize(old)), r.join(sanitize(new)));
    if from.is_dir() && !to.exists() {
        let _ = std::fs::rename(from, to);
    }
}

/// Textdatei lesen: UTF-8 (mit/ohne BOM), UTF-16 LE mit BOM, sonst Windows-1252.
pub fn decode(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = b.strip_prefix(&[0xFF, 0xFE]) {
        let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        // Navicat 8 speichert im ANSI-Zeichensatz (Windows-1252)
        Err(_) => b.iter().map(|&c| cp1252(c)).collect(),
    }
}

pub fn cp1252(c: u8) -> char {
    const HI: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘', '’', '“', '”',
        '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match c {
        0x80..=0x9F => HI[(c - 0x80) as usize],
        _ => c as char,
    }
}

pub fn fmt_time(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    // Unix-Zeit → Datum (UTC), ohne zusätzliche Abhängigkeit
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Tage seit 1970-01-01 → (Jahr, Monat, Tag) (Algorithmus von H. Hinnant).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_decoding() {
        assert_eq!(sanitize("a/b:c?"), "a_b_c_");
        assert_eq!(sanitize("  .. "), "_");
        assert_eq!(decode(b"\xEF\xBB\xBFSELECT 1"), "SELECT 1");
        assert_eq!(decode(b"SELECT 'Gr\xFC\xDFe'"), "SELECT 'Grüße'");
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_000), (2024, 10, 4));
    }
}
