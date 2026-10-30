#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! MariaDB Manager – schlanker, schneller DB-Navigator im Stil von Navicat 8.

mod app;
mod cellview;
mod config;
mod datatab;
mod designtab;
mod export;
mod db;
mod grid;
mod icons;
mod import;
mod monitor;
mod navicat;
mod queries;
mod querytab;
mod sqlhl;
mod users;

use eframe::egui;

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
        style.visuals.faint_bg_color = egui::Color32::from_gray(238);
    });
}

/// Setzt Arial (bzw. eine ähnliche Systemschrift) als Proportionalschrift.
/// Die Schrift wird zur Laufzeit gelesen, nicht eingebettet; die egui-Standardschriften
/// bleiben als Fallback (u. a. für die Icons) erhalten.
pub fn install_fonts(ctx: &egui::Context) {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if cfg!(windows) {
        let win = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        let fonts = std::path::Path::new(&win).join("Fonts");
        candidates.push(fonts.join("arial.ttf"));
        candidates.push(fonts.join("segoeui.ttf"));
    } else if cfg!(target_os = "macos") {
        candidates.push("/System/Library/Fonts/Supplemental/Arial.ttf".into());
        candidates.push("/Library/Fonts/Arial.ttf".into());
    } else {
        for p in [
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
            "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
            "/usr/share/fonts/liberation-sans-fonts/LiberationSans-Regular.ttf",
            "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/msttcorefonts/Arial.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ] {
            candidates.push(p.into());
        }
    }
    let Some(data) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("system-sans".into(), std::sync::Arc::new(egui::FontData::from_owned(data)));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "system-sans".into());
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
