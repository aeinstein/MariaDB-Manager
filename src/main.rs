#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MariaDB Manager – schlanker, schneller DB-Navigator im Stil von Navicat 8.

mod app;
mod backup;
mod cellview;
mod config;
mod datatab;
mod designtab;
mod export;
mod fontscan;
#[cfg(windows)]
mod gdifont;
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

/// Setzt die gewählte installierte Schrift (leer/unbekannt = Standard, meist Arial) als
/// Proportionalschrift. Die Schrift wird zur Laufzeit gelesen, nicht eingebettet; die
/// egui-Standardschriften bleiben als Fallback (u. a. für die Icons) erhalten.
/// `gdi` = unter Windows die Glyphen per GDI rastern lassen, mit dieser Schriftstärke (siehe `gdifont`).
pub fn install_fonts(ctx: &egui::Context, name: &str, gdi: Option<f32>) {
    // Gewählte Schrift; ist sie nicht lesbar (z. B. deinstalliert), die Standardschrift
    let found = fontscan::find(name)
        .into_iter()
        .chain(fontscan::find(""))
        .find_map(|f| std::fs::read(&f.path).ok().map(|d| (f.name.as_str(), f.index, d)));
    #[cfg(windows)]
    gdifont::install(ctx, found.as_ref().filter(|_| gdi.is_some()).map(|(n, ..)| *n), gdi.unwrap_or(0.0));
    #[cfg(not(windows))]
    let _ = gdi;
    let mut fonts = egui::FontDefinitions::default();
    if let Some((_, index, data)) = found {
        // Normales Hinting der Schrift (egui-Standard). Der Modus „Mono“ (volles TrueType-Hinting)
        // wurde getestet: dünne, ungleichmäßig verteilte Buchstaben – deutlich schlechter.
        let fd = egui::FontData::from_blob(std::sync::Arc::new(data), index);
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
            .with_min_inner_size([720.0, 420.0])
            .with_app_id("mariadb-manager")
            .with_icon(app_icon().unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native("MariaDB Manager", opts, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}

/// App-Icon für Fenster und Taskleiste (aus `assets/icon.png`, erzeugt mit `tools/gen-icon`).
pub fn app_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory_with_format(include_bytes!("../assets/icon.png"), image::ImageFormat::Png).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width, height })
}
