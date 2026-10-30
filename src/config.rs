//! Speichert Verbindungen und Einstellungen als JSON im Benutzer-Konfigurationsordner.

use crate::db::ConnConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub connections: Vec<ConnConfig>,
    /// Zeilen pro Seite im Datengrid.
    pub page_size: usize,
    /// Maximale Zeilen pro Ergebnis im SQL-Editor.
    pub max_query_rows: usize,
    /// Hell/Dunkel; Standard: dem Betriebssystem folgen.
    pub theme: ThemeMode,
    pub font_scale: f32,
    /// Schriftart der Oberfläche (leer = Standard, meist Arial).
    pub font: String,
    /// Schärfere Schrift: ganzzahlige Pixelgrößen, kein Sub-Pixel-Binning, kräftigere Kanten.
    pub sharp_text: bool,
    /// Zeilenhöhe im Datengrid (Pixel).
    pub grid_row_h: f32,
    /// CSV-Export mit UTF-8-BOM.
    pub csv_bom: bool,
    /// Wurde der Navicat-Import beim Start schon angeboten?
    pub navicat_asked: bool,
    /// Zuletzt gewählte Optionen für den SQL-Dump.
    pub dump: crate::dump::DumpOpts,
    /// TelliMATE-Modus: Tabellen `vorgaenge_*` / `helper_*` / `tarife_*` im Baum als Unterordner.
    pub tellimate: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "Wie System",
            ThemeMode::Light => "Hell",
            ThemeMode::Dark => "Dunkel",
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self { connections: Vec::new(), page_size: 1000, max_query_rows: 100_000, theme: ThemeMode::System, font_scale: 1.0, font: String::new(), sharp_text: true, grid_row_h: 20.0, csv_bom: true, navicat_asked: false, dump: Default::default(), tellimate: false }
    }
}

/// Konfigurationsordner (Einstellungen, gespeicherte Abfragen).
pub fn data_dir() -> Option<PathBuf> {
    let d = directories::ProjectDirs::from("de", "itnox", "MariaDB Manager")?;
    Some(d.config_dir().to_path_buf())
}

fn path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let p = path().ok_or("Kein Konfigurationsordner gefunden")?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut s = self.clone();
        for c in &mut s.connections {
            if !c.save_password {
                c.password.clear();
            }
        }
        let json = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
    }

    pub fn location() -> String {
        path().map(|p| p.display().to_string()).unwrap_or_default()
    }
}
