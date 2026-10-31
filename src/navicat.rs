//! Import aus Navicat 8 (Windows): Verbindungen aus der Registry
//! (`HKCU\Software\PremiumSoft\Navicat\Servers`) und gespeicherte Abfragen
//! (`.sql`-Dateien im `QuerySavePath`, ggf. in den VirtualStore umgeleitet).

use crate::db::ConnConfig;
use std::path::{Path, PathBuf};

pub struct NavConn {
    pub cfg: ConnConfig,
    pub query_dir: Option<PathBuf>,
    pub queries: usize,
    pub ssh: bool,
}

#[derive(Default)]
pub struct Report {
    pub added: Vec<String>,
    pub skipped: Vec<String>,
    pub queries: usize,
    pub ssh: Vec<String>,
    pub errors: Vec<String>,
}

impl Report {
    pub fn text(&self) -> String {
        let mut s = format!("{} Verbindung(en) importiert, {} Abfrage(n) übernommen.", self.added.len(), self.queries);
        if !self.skipped.is_empty() {
            s.push_str(&format!("\nBereits vorhanden (nur Abfragen ergänzt): {}", self.skipped.join(", ")));
        }
        if !self.ssh.is_empty() {
            s.push_str(&format!("\nSSH-Tunnel wird nicht unterstützt – bitte prüfen: {}", self.ssh.join(", ")));
        }
        for e in &self.errors {
            s.push_str(&format!("\n{e}"));
        }
        s
    }
}

// ------------------------------------------------------------------ Passwort

/// Navicat ≤ 11: Blowfish mit Schlüssel SHA1("3DC5CA39") in einem eigenen CBC-Modus.
mod crypt {
    use blowfish::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
    use blowfish::Blowfish;
    use sha1::{Digest, Sha1};

    fn cipher() -> Blowfish {
        let key = Sha1::digest(b"3DC5CA39");
        Blowfish::new_from_slice(&key).expect("Schlüssellänge gültig")
    }

    fn enc(c: &Blowfish, b: [u8; 8]) -> [u8; 8] {
        let mut g = GenericArray::from(b);
        c.encrypt_block(&mut g);
        g.into()
    }

    fn xor(a: [u8; 8], b: [u8; 8]) -> [u8; 8] {
        std::array::from_fn(|i| a[i] ^ b[i])
    }

    pub fn decrypt(hex: &str) -> Option<String> {
        let ct = crate::cellview::decode_hex(hex.trim());
        if ct.len() * 2 != hex.trim().len() {
            return None;
        }
        let c = cipher();
        let mut cv = enc(&c, [0xFF; 8]);
        let mut out = Vec::with_capacity(ct.len());
        for chunk in ct.chunks(8) {
            if chunk.len() == 8 {
                let blk: [u8; 8] = chunk.try_into().ok()?;
                let mut g = GenericArray::from(blk);
                c.decrypt_block(&mut g);
                out.extend_from_slice(&xor(g.into(), cv));
                cv = xor(cv, blk);
            } else {
                let e = enc(&c, cv);
                out.extend(chunk.iter().zip(e.iter()).map(|(a, b)| a ^ b));
            }
        }
        String::from_utf8(out).ok()
    }

    #[cfg(test)]
    pub fn encrypt(plain: &str) -> String {
        let c = cipher();
        let mut cv = enc(&c, [0xFF; 8]);
        let mut out = Vec::new();
        for chunk in plain.as_bytes().chunks(8) {
            if chunk.len() == 8 {
                let blk: [u8; 8] = chunk.try_into().unwrap();
                let t = enc(&c, xor(blk, cv));
                out.extend_from_slice(&t);
                cv = xor(cv, t);
            } else {
                let e = enc(&c, cv);
                out.extend(chunk.iter().zip(e.iter()).map(|(a, b)| a ^ b));
            }
        }
        out.iter().map(|b| format!("{b:02X}")).collect()
    }
}

pub use crypt::decrypt;

// ------------------------------------------------------------------ Suchen

/// Navicat legt Dateien unter „Program Files“ an; ohne Admin-Rechte landen sie im VirtualStore.
fn resolve_dir(p: &str) -> Option<PathBuf> {
    let p = p.trim();
    if p.is_empty() {
        return None;
    }
    let direct = PathBuf::from(p);
    if direct.is_dir() {
        return Some(direct);
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    // „C:\Program Files (x86)\…“ → „%LOCALAPPDATA%\VirtualStore\Program Files (x86)\…“
    let rest = p.get(3..).filter(|_| p.get(1..3) == Some(":\\"))?;
    let vs = Path::new(&local).join("VirtualStore").join(rest);
    vs.is_dir().then_some(vs)
}

/// Anzahl `.sql`-Dateien in den Datenbank-Unterordnern.
fn count_queries(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| sql_files(&e.path()).len())
        .sum()
}

fn sql_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sql")))
                .collect()
        })
        .unwrap_or_default()
}

/// Gefundene Navicat-Verbindungen (leer, wenn Navicat nicht installiert ist).
#[cfg(windows)]
pub fn find() -> Vec<NavConn> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let Ok(servers) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\PremiumSoft\Navicat\Servers") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in servers.enum_keys().flatten() {
        let Ok(k) = servers.open_subkey(&name) else { continue };
        let s = |v: &str| k.get_value::<String, _>(v).unwrap_or_default();
        let n = |v: &str| k.get_value::<u32, _>(v).ok().or_else(|| s(v).trim().parse().ok());
        let ct = s("ConnType");
        if !ct.is_empty() && !ct.eq_ignore_ascii_case("ctMYSQL") {
            continue;
        }
        let pwd = s("Pwd");
        let password = if pwd.is_empty() { String::new() } else { decrypt(&pwd).unwrap_or_default() };
        let query_dir = resolve_dir(&s("QuerySavePath"));
        out.push(NavConn {
            queries: query_dir.as_deref().map(count_queries).unwrap_or(0),
            query_dir,
            ssh: n("UseSSH").unwrap_or(0) != 0,
            cfg: ConnConfig {
                name: name.clone(),
                host: s("Host"),
                port: n("Port").and_then(|p| u16::try_from(p).ok()).filter(|p| *p > 0).unwrap_or(3306),
                user: s("UserName"),
                save_password: !password.is_empty(),
                password,
                default_db: s("InitialDatabase"),
                use_ssl: n("UseSSL").unwrap_or(0) != 0,
                ssl_verify: false,
                show_system_dbs: false,
            },
        });
    }
    out.sort_by_key(|c| c.cfg.name.to_lowercase());
    out
}

#[cfg(not(windows))]
pub fn find() -> Vec<NavConn> {
    Vec::new()
}

/// Übernimmt die gewählten Verbindungen (ohne bereits vorhandene Namen) und kopiert
/// ihre Abfragen (vorhandene Dateien werden nicht überschrieben).
pub fn import(sel: &[&NavConn], existing: &[String]) -> (Vec<ConnConfig>, Report) {
    let mut rep = Report::default();
    let mut new = Vec::new();
    for nc in sel {
        let name = nc.cfg.name.clone();
        if existing.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
            rep.skipped.push(name.clone());
        } else {
            new.push(nc.cfg.clone());
            rep.added.push(name.clone());
        }
        if nc.ssh {
            rep.ssh.push(name.clone());
        }
        let Some(dir) = &nc.query_dir else { continue };
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for db_dir in rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
            let db = db_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            for f in sql_files(&db_dir) {
                let q = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if crate::queries::exists(&name, &db, &q) {
                    continue;
                }
                match std::fs::read(&f) {
                    Ok(b) => match crate::queries::save(&name, &db, &q, &crate::queries::decode(&b)) {
                        Ok(()) => rep.queries += 1,
                        Err(e) => rep.errors.push(e),
                    },
                    Err(e) => rep.errors.push(format!("{}: {e}", f.display())),
                }
            }
        }
    }
    (new, rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manuell: `cargo test navicat_scan -- --ignored --nocapture` (liest nur, gibt keine Passwörter aus).
    #[test]
    #[ignore]
    fn navicat_scan() {
        let f = find();
        let pw_set = f.iter().filter(|c| !c.cfg.password.is_empty()).count();
        let pw_ok = f.iter().filter(|c| c.cfg.password.chars().all(|ch| !ch.is_control())).count();
        let q: usize = f.iter().map(|c| c.queries).sum();
        let dirs = f.iter().filter(|c| c.query_dir.is_some()).count();
        let ssh = f.iter().filter(|c| c.ssh).count();
        println!("Verbindungen: {}, mit Passwort: {pw_set}, Passwort lesbar: {pw_ok}, Abfrage-Ordner: {dirs}, Abfragen: {q}, SSH: {ssh}", f.len());
        for c in &f {
            println!("  {} – {}@{}:{} db='{}' ssl={} abfragen={}", c.cfg.name, c.cfg.user, c.cfg.host, c.cfg.port, c.cfg.default_db, c.cfg.use_ssl, c.queries);
        }
    }

    #[test]
    fn password_roundtrip() {
        // Referenzwert aus der bekannten Navicat-11-Implementierung
        assert_eq!(crypt::encrypt("This is a test"), "0EA71F51DD37BFB60CCBA219BE3A");
        for p in ["", "a", "geheim", "12345678", "Ein längeres Passwort!"] {
            assert_eq!(decrypt(&crypt::encrypt(p)).as_deref(), Some(p));
        }
        assert_eq!(decrypt("0EA71F51DD37BFB60CCBA219BE3A").as_deref(), Some("This is a test"));
    }
}
