#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MariaDB Manager – schlanker, schneller DB-Navigator im Stil von Navicat 8.

mod app;
mod backup;
mod cellview;
mod config;
mod datatab;
mod designtab;
mod export;
mod db;
mod dump;
mod grid;
mod icons;
mod import;
mod monitor;
mod navicat;
mod queries;
mod querytab;
mod sqlhl;
mod sync;
mod users;

use eframe::egui;

/// Schriftgrößen so runden, dass sie in physischen Pixeln ganzzahlig sind (z. B. 13 px bei 100 %,
/// 16 px bei 125 %) – das Hinting kann nur dann sauber einrasten. Muss bei jedem Wechsel der
/// Pixeldichte (anderer Monitor, Zoom) neu aufgerufen werden; `sharp = false` = egui-Standardgrößen.
pub fn snap_text_sizes(ctx: &egui::Context, sharp: bool) {
    let ppp = ctx.pixels_per_point();
    let defaults = egui::Style::default().text_styles;
    ctx.all_styles_mut(|st| {
        for (ts, fid) in st.text_styles.iter_mut() {
            let base = defaults.get(ts).map_or(fid.size, |d| d.size);
            fid.size = if sharp { (base * ppp).round() / ppp } else { base };
        }
    });
}

pub fn apply_style(ctx: &egui::Context, s: &config::Settings) {
    ctx.set_theme(match s.theme {
        config::ThemeMode::System => egui::ThemePreference::System,
        config::ThemeMode::Light => egui::ThemePreference::Light,
        config::ThemeMode::Dark => egui::ThemePreference::Dark,
    });
    ctx.set_zoom_factor(s.font_scale.clamp(0.6, 2.0));
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(6.0, 3.0);
        style.spacing.interact_size.y = 20.0;
        style.interaction.selectable_labels = false;
        // Kein weiches Ausblenden am Rand der Scrollbereiche
        style.spacing.scroll.fade.strength = 0.0;
        // Scharfe Schrift: Glyphen an ganzen Pixeln statt an Bruchteil-Positionen
        style.visuals.text_options.font_hinting = true;
        style.visuals.text_options.subpixel_binning = !s.sharp_text;
        // Kantig wie Navicat 8: nirgends abgerundete Ecken
        let v = &mut style.visuals;
        v.window_corner_radius = egui::CornerRadius::ZERO;
        v.menu_corner_radius = egui::CornerRadius::ZERO;
        let w = &mut v.widgets;
        for ws in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            ws.corner_radius = egui::CornerRadius::ZERO;
        }
    });
    // Im hellen Design Textfarben etwas kräftiger
    ctx.style_mut_of(egui::Theme::Light, |style| {
        let w = &mut style.visuals.widgets;
        w.noninteractive.fg_stroke.color = egui::Color32::from_gray(25);
        w.inactive.fg_stroke.color = egui::Color32::from_gray(20);
        w.hovered.fg_stroke.color = egui::Color32::BLACK;
        w.active.fg_stroke.color = egui::Color32::BLACK;
        style.visuals.panel_fill = egui::Color32::from_gray(246);
        // Glättungsränder kräftiger – dunkler und klarer, näher an ClearType
        // (bei 100 % und 125 % per Screenshot-Vergleich ermittelt)
        style.visuals.text_options.color_transfer_function = if s.sharp_text {
            egui::epaint::FontColorTransferFunction::Gamma(0.7)
        } else {
            egui::epaint::FontColorTransferFunction::LIGHT_MODE_DEFAULT
        };
        style.visuals.faint_bg_color = egui::Color32::from_gray(238);
    });
}

/// Wählbare Systemschriften (Name, mögliche Dateipfade). Die erste vorhandene ist der Standard.
fn font_choices() -> Vec<(&'static str, Vec<std::path::PathBuf>)> {
    if cfg!(windows) {
        let win = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        let dir = std::path::Path::new(&win).join("Fonts");
        [
            ("Arial", "arial.ttf"),
            ("Tahoma", "tahoma.ttf"),
            ("Verdana", "verdana.ttf"),
            ("Microsoft Sans Serif", "micross.ttf"),
            ("Segoe UI", "segoeui.ttf"),
            ("Calibri", "calibri.ttf"),
            ("Trebuchet MS", "trebuc.ttf"),
        ]
        .into_iter()
        .map(|(n, f)| (n, vec![dir.join(f)]))
        .collect()
    } else if cfg!(target_os = "macos") {
        let sup = |f: &str| vec![format!("/System/Library/Fonts/Supplemental/{f}").into(), format!("/Library/Fonts/{f}").into()];
        vec![
            ("Arial", sup("Arial.ttf")),
            ("Tahoma", sup("Tahoma.ttf")),
            ("Verdana", sup("Verdana.ttf")),
            ("Trebuchet MS", sup("Trebuchet MS.ttf")),
        ]
    } else {
        let p = |v: &[&str]| v.iter().map(|s| std::path::PathBuf::from(*s)).collect::<Vec<_>>();
        vec![
            (
                "Liberation Sans",
                p(&[
                    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/liberation-sans-fonts/LiberationSans-Regular.ttf",
                    "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
                ]),
            ),
            ("Arial", p(&["/usr/share/fonts/truetype/msttcorefonts/Arial.ttf"])),
            ("Tahoma", p(&["/usr/share/fonts/truetype/msttcorefonts/tahoma.ttf", "/usr/share/wine/fonts/tahoma.ttf"])),
            ("Verdana", p(&["/usr/share/fonts/truetype/msttcorefonts/Verdana.ttf"])),
            ("DejaVu Sans", p(&["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf"])),
            ("Noto Sans", p(&["/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf", "/usr/share/fonts/noto/NotoSans-Regular.ttf"])),
        ]
    }
}

/// Namen der auf diesem System vorhandenen Schriften (für die Auswahl in den Optionen).
pub fn available_fonts() -> Vec<&'static str> {
    font_choices().into_iter().filter(|(_, p)| p.iter().any(|p| p.exists())).map(|(n, _)| n).collect()
}

/// Setzt die gewählte Systemschrift (leer/unbekannt = erste vorhandene, meist Arial) als
/// Proportionalschrift. Die Schrift wird zur Laufzeit gelesen, nicht eingebettet; die
/// egui-Standardschriften bleiben als Fallback (u. a. für die Icons) erhalten.
pub fn install_fonts(ctx: &egui::Context, name: &str) {
    let choices = font_choices();
    let read = |paths: &[std::path::PathBuf]| paths.iter().find_map(|p| std::fs::read(p).ok());
    let data = choices
        .iter()
        .filter(|(n, _)| *n == name)
        .chain(choices.iter())
        .find_map(|(_, p)| read(p));
    let mut fonts = egui::FontDefinitions::default();
    if let Some(data) = data {
        // Normales Hinting der Schrift (egui-Standard). Der Modus „Mono“ (volles TrueType-Hinting)
        // wurde getestet: dünne, ungleichmäßig verteilte Buchstaben – deutlich schlechter.
        let fd = egui::FontData::from_owned(data);
        fonts.font_data.insert("system-sans".into(), std::sync::Arc::new(fd));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "system-sans".into());
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MariaDB Manager")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("MariaDB Manager", opts, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
