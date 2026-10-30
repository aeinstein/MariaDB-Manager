//! Farbige Vektor-Icons im Stil von Navicat 8, direkt mit dem egui-Painter gezeichnet
//! (keine Bilddateien). Entworfen auf einem 16×16-Raster, beliebig skalierbar.

use eframe::egui::{self, pos2, vec2, Align2, Atom, Color32, FontId, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind};
use std::f32::consts::PI;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    ConnOpen,
    ConnClosed,
    ConnBusy,
    ConnFailed,
    NewConn,
    Database,
    DatabaseClosed,
    NewDatabase,
    Table,
    View,
    Procedure,
    Function,
    Event,
    Query,
    NewQuery,
    User,
    NewUser,
    Options,
    Objects,
    Open,
    Ddl,
    Truncate,
    Delete,
    Refresh,
    Copy,
    Search,
    Edit,
    Add,
    Run,
    Save,
    Design,
    NewTable,
    Key,
    Up,
    Down,
    AddField,
    InsertField,
    DeleteField,
    Export,
    Import,
}

/// Atom für Buttons/Labels: `Button::new((icons::atom(Icon::Table, 16.0), "Text"))`.
pub fn atom<'a>(icon: Icon, size: f32) -> Atom<'a> {
    Atom::paint(vec2(size, size), move |ui, a| paint(ui.painter(), a.rect, icon, ui.is_enabled()))
}

/// Button mit Icon und Text.
pub fn button<'a>(icon: Icon, text: impl Into<egui::WidgetText>) -> egui::Button<'a> {
    let text: egui::WidgetText = text.into();
    egui::Button::new((atom(icon, 16.0), text))
}

/// Icon als eigenständiges Element.
pub fn show(ui: &mut egui::Ui, icon: Icon, size: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect, icon, ui.is_enabled());
    }
    resp
}

const WHITE: Color32 = Color32::WHITE;
const BLUE_DARK: Color32 = Color32::from_rgb(0x3a, 0x6e, 0xa5);
const BLUE: Color32 = Color32::from_rgb(0x5b, 0x8f, 0xd0);
const BLUE_GRID: Color32 = Color32::from_rgb(0xa8, 0xc4, 0xe8);
const GREEN: Color32 = Color32::from_rgb(0x4f, 0xa6, 0x32);
const GREEN_LIGHT: Color32 = Color32::from_rgb(0xa6, 0xdc, 0x6e);
const GREEN_DARK: Color32 = Color32::from_rgb(0x2c, 0x6b, 0x1a);
const GREY: Color32 = Color32::from_rgb(0xa8, 0xa8, 0xa8);
const GREY_LIGHT: Color32 = Color32::from_rgb(0xdc, 0xdc, 0xdc);
const GREY_DARK: Color32 = Color32::from_rgb(0x6c, 0x6c, 0x6c);
const RED: Color32 = Color32::from_rgb(0xd0, 0x3a, 0x2e);
const ORANGE: Color32 = Color32::from_rgb(0xe8, 0x9a, 0x17);
const ORANGE_DARK: Color32 = Color32::from_rgb(0xa8, 0x66, 0x0a);
const YELLOW: Color32 = Color32::from_rgb(0xf3, 0xc4, 0x4a);
const YELLOW_DARK: Color32 = Color32::from_rgb(0xb8, 0x89, 0x1d);
const PURPLE: Color32 = Color32::from_rgb(0x7a, 0x4b, 0xb0);
const SKIN: Color32 = Color32::from_rgb(0xf5, 0xc0, 0x8a);
const SKIN_DARK: Color32 = Color32::from_rgb(0xb5, 0x79, 0x3f);

/// Zeichenhilfe: Koordinaten im 16er-Raster, auf `rect` skaliert.
struct Pen<'a> {
    p: &'a Painter,
    o: Pos2,
    s: f32,
    en: bool,
}

impl Pen<'_> {
    fn pt(&self, x: f32, y: f32) -> Pos2 {
        self.o + vec2(x, y) * self.s
    }
    fn c(&self, c: Color32) -> Color32 {
        if self.en {
            c
        } else {
            let l = ((c.r() as u32 * 30 + c.g() as u32 * 59 + c.b() as u32 * 11) / 100) as u8;
            Color32::from_gray(l).gamma_multiply(0.5)
        }
    }
    fn st(&self, w: f32, c: Color32) -> Stroke {
        Stroke::new(w * self.s, self.c(c))
    }
    fn rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, fill: Color32, stroke: Color32) {
        let r = Rect::from_min_max(self.pt(x0, y0), self.pt(x1, y1));
        self.p.rect(r, 0.0, self.c(fill), self.st(1.0, stroke), StrokeKind::Inside);
    }
    fn fill(&self, x0: f32, y0: f32, x1: f32, y1: f32, fill: Color32) {
        self.p.rect_filled(Rect::from_min_max(self.pt(x0, y0), self.pt(x1, y1)), 0.0, self.c(fill));
    }
    fn line(&self, pts: &[(f32, f32)], w: f32, c: Color32) {
        self.p.add(Shape::line(pts.iter().map(|&(x, y)| self.pt(x, y)).collect(), self.st(w, c)));
    }
    fn poly(&self, pts: &[(f32, f32)], fill: Color32, stroke: Color32) {
        let v = pts.iter().map(|&(x, y)| self.pt(x, y)).collect();
        self.p.add(Shape::convex_polygon(v, self.c(fill), self.st(1.0, stroke)));
    }
    fn circle(&self, x: f32, y: f32, r: f32, fill: Color32, stroke: Color32) {
        self.p.circle(self.pt(x, y), r * self.s, self.c(fill), self.st(1.0, stroke));
    }
    fn ellipse(&self, x: f32, y: f32, rx: f32, ry: f32, fill: Color32, stroke: Option<Color32>) {
        let (c, r) = (self.pt(x, y), vec2(rx, ry) * self.s);
        self.p.add(Shape::ellipse_filled(c, r, self.c(fill)));
        if let Some(sc) = stroke {
            self.p.add(Shape::ellipse_stroke(c, r, self.st(1.0, sc)));
        }
    }
    /// Ellipsenbogen von Winkel `a0` bis `a1` (Bogenmaß, y nach unten).
    fn arc(&self, x: f32, y: f32, rx: f32, ry: f32, a0: f32, a1: f32, w: f32, c: Color32) {
        let n = 16;
        let pts = (0..=n)
            .map(|i| {
                let t = a0 + (a1 - a0) * i as f32 / n as f32;
                self.pt(x + rx * t.cos(), y + ry * t.sin())
            })
            .collect();
        self.p.add(Shape::line(pts, self.st(w, c)));
    }
    fn text(&self, x: f32, y: f32, size: f32, t: &str, c: Color32) {
        self.p.text(self.pt(x, y), Align2::CENTER_CENTER, t, FontId::proportional(size * self.s), self.c(c));
    }

    // ---- Bausteine

    /// Tabelle: blauer Kopf, weißes Raster.
    fn table(&self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.rect(x0, y0, x1, y1, WHITE, BLUE_DARK);
        let hy = y0 + (y1 - y0) * 0.28;
        self.fill(x0, y0, x1, hy, BLUE);
        self.rect(x0, y0, x1, y1, Color32::TRANSPARENT, BLUE_DARK);
        let (w, h) = (x1 - x0, y1 - hy);
        for k in 1..3 {
            let y = hy + h * k as f32 / 3.0;
            self.line(&[(x0 + 1.0, y), (x1 - 1.0, y)], 1.0, BLUE_GRID);
        }
        for k in 1..3 {
            let x = x0 + w * k as f32 / 3.0;
            self.line(&[(x, hy), (x, y1 - 1.0)], 1.0, BLUE_GRID);
        }
    }

    /// Datenbank-Zylinder.
    fn cyl(&self, x0: f32, y0: f32, x1: f32, y1: f32, body: Color32, top: Color32, dark: Color32) {
        let (cx, rx) = ((x0 + x1) / 2.0, (x1 - x0) / 2.0);
        let ry = rx * 0.4;
        self.fill(x0, y0 + ry, x1, y1 - ry, body);
        self.ellipse(cx, y1 - ry, rx, ry, body, None);
        self.arc(cx, y1 - ry, rx, ry, 0.0, PI, 1.0, dark);
        self.arc(cx, (y0 + y1) / 2.0, rx, ry, 0.0, PI, 1.0, dark.gamma_multiply(0.6));
        self.line(&[(x0, y0 + ry), (x0, y1 - ry)], 1.0, dark);
        self.line(&[(x1, y0 + ry), (x1, y1 - ry)], 1.0, dark);
        self.ellipse(cx, y0 + ry, rx, ry, top, Some(dark));
    }

    fn gear(&self, x: f32, y: f32, r: f32, fill: Color32, dark: Color32) {
        for k in 0..8 {
            let a = k as f32 * PI / 4.0;
            self.circle(x + r * 0.85 * a.cos(), y + r * 0.85 * a.sin(), r * 0.27, fill, Color32::TRANSPARENT);
        }
        self.circle(x, y, r * 0.72, fill, dark);
        self.circle(x, y, r * 0.28, WHITE, dark);
    }

    fn clock(&self, x: f32, y: f32, r: f32) {
        self.circle(x, y, r, WHITE, BLUE_DARK);
        self.line(&[(x, y - r * 0.65), (x, y), (x + r * 0.5, y + r * 0.2)], 1.0, GREY_DARK);
    }

    fn plus(&self, x: f32, y: f32) {
        self.circle(x, y, 3.6, GREEN, GREEN_DARK);
        self.line(&[(x - 2.0, y), (x + 2.0, y)], 1.4, WHITE);
        self.line(&[(x, y - 2.0), (x, y + 2.0)], 1.4, WHITE);
    }

    fn conn(&self, line: Color32) {
        self.rect(1.0, 2.0, 15.0, 14.0, WHITE, GREY_DARK);
        self.fill(1.0, 2.0, 15.0, 4.0, GREY_LIGHT);
        self.rect(1.0, 2.0, 15.0, 14.0, Color32::TRANSPARENT, GREY_DARK);
        self.line(&[(3.0, 12.0), (6.0, 8.5), (9.0, 10.0), (13.0, 5.5)], 1.6, line);
    }

    fn page(&self, x0: f32, y0: f32, x1: f32, y1: f32) {
        self.rect(x0, y0, x1, y1, WHITE, GREY_DARK);
        let mut y = y0 + 3.0;
        while y < y1 - 1.5 {
            self.line(&[(x0 + 2.0, y), (x1 - 2.0, y)], 1.0, BLUE);
            y += 2.0;
        }
    }

    fn user(&self, x: f32, s: f32) {
        // Schultern als Halbellipse
        let pts: Vec<(f32, f32)> = (0..=12).map(|i| {
            let t = PI + PI * i as f32 / 12.0;
            (x + 6.0 * s * t.cos(), 15.0 + 6.0 * s * t.sin())
        }).collect();
        self.poly(&pts, ORANGE, ORANGE_DARK);
        self.circle(x, 15.0 - 9.5 * s, 3.3 * s, SKIN, SKIN_DARK);
    }
}

pub fn paint(p: &Painter, rect: Rect, icon: Icon, enabled: bool) {
    let side = rect.width().min(rect.height());
    let s = side / 16.0;
    let o = (rect.center() - vec2(8.0, 8.0) * s).round();
    let d = Pen { p, o: pos2(o.x, o.y), s, en: enabled };
    match icon {
        Icon::ConnOpen => d.conn(GREEN),
        Icon::ConnClosed => d.conn(RED),
        Icon::ConnBusy => d.conn(ORANGE),
        Icon::ConnFailed => {
            d.conn(RED);
            d.poly(&[(11.5, 8.0), (15.5, 15.0), (7.5, 15.0)], YELLOW, ORANGE_DARK);
            d.line(&[(11.5, 10.5), (11.5, 12.6)], 1.0, Color32::BLACK);
            d.line(&[(11.5, 13.4), (11.5, 14.2)], 1.0, Color32::BLACK);
        }
        Icon::NewConn => {
            d.conn(GREEN);
            d.plus(12.0, 12.0);
        }
        Icon::Database => d.cyl(2.5, 1.0, 13.5, 15.0, GREEN, GREEN_LIGHT, GREEN_DARK),
        Icon::DatabaseClosed => d.cyl(2.5, 1.0, 13.5, 15.0, GREY, GREY_LIGHT, GREY_DARK),
        Icon::NewDatabase => {
            d.cyl(1.0, 1.0, 11.0, 14.0, GREEN, GREEN_LIGHT, GREEN_DARK);
            d.plus(12.0, 12.0);
        }
        Icon::Table => d.table(1.0, 2.0, 15.0, 14.0),
        Icon::View => {
            d.table(1.0, 1.0, 14.0, 11.0);
            d.circle(5.5, 13.0, 2.2, Color32::from_rgb(0xd8, 0xec, 0xff), GREY_DARK);
            d.circle(11.0, 13.0, 2.2, Color32::from_rgb(0xd8, 0xec, 0xff), GREY_DARK);
            d.line(&[(7.7, 12.6), (8.8, 12.6)], 1.0, GREY_DARK);
        }
        Icon::Procedure => {
            d.cyl(1.0, 1.0, 11.0, 13.0, GREEN, GREEN_LIGHT, GREEN_DARK);
            d.gear(11.5, 11.5, 4.3, ORANGE, ORANGE_DARK);
        }
        Icon::Function => {
            d.rect(1.0, 2.0, 15.0, 14.0, WHITE, PURPLE);
            d.fill(1.0, 2.0, 15.0, 4.5, PURPLE);
            d.text(8.0, 9.5, 7.5, "f(x)", PURPLE);
        }
        Icon::Event => {
            d.rect(1.0, 2.0, 13.0, 14.0, WHITE, GREY_DARK);
            d.fill(1.0, 2.0, 13.0, 5.0, RED);
            d.rect(1.0, 2.0, 13.0, 14.0, Color32::TRANSPARENT, GREY_DARK);
            for (x, y) in [(3.5, 7.5), (6.5, 7.5), (9.5, 7.5), (3.5, 10.5), (6.5, 10.5)] {
                d.fill(x - 0.8, y - 0.8, x + 0.8, y + 0.8, BLUE_GRID);
            }
            d.clock(11.5, 11.5, 4.0);
        }
        Icon::Query => {
            d.table(5.0, 1.0, 15.0, 10.0);
            d.table(1.0, 6.0, 11.0, 15.0);
        }
        Icon::NewQuery => {
            d.table(1.0, 1.0, 12.0, 12.0);
            d.plus(12.0, 12.0);
        }
        Icon::User => d.user(8.0, 1.0),
        Icon::NewUser => {
            d.user(6.5, 0.9);
            d.plus(12.0, 12.0);
        }
        Icon::Options => d.gear(8.0, 8.0, 6.8, GREY, GREY_DARK),
        Icon::Objects => {
            d.rect(1.0, 1.0, 7.5, 7.5, BLUE, BLUE_DARK);
            d.rect(8.5, 1.0, 15.0, 7.5, GREEN, GREEN_DARK);
            d.rect(1.0, 8.5, 7.5, 15.0, ORANGE, ORANGE_DARK);
            d.rect(8.5, 8.5, 15.0, 15.0, PURPLE, PURPLE);
        }
        Icon::Open => {
            d.poly(&[(1.0, 3.0), (6.0, 3.0), (7.5, 5.0), (1.0, 5.0)], YELLOW_DARK, YELLOW_DARK);
            d.rect(1.0, 5.0, 15.0, 14.0, YELLOW, YELLOW_DARK);
        }
        Icon::Ddl => d.page(3.0, 1.0, 13.0, 15.0),
        Icon::Truncate => {
            d.table(1.0, 1.0, 13.0, 12.0);
            d.circle(12.0, 12.0, 3.6, RED, Color32::from_rgb(0x8a, 0x1e, 0x16));
            d.line(&[(10.0, 12.0), (14.0, 12.0)], 1.4, WHITE);
        }
        Icon::Delete => {
            d.line(&[(3.0, 3.0), (13.0, 13.0)], 2.6, RED);
            d.line(&[(13.0, 3.0), (3.0, 13.0)], 2.6, RED);
        }
        Icon::Refresh => {
            d.arc(8.0, 8.0, 5.5, 5.5, -PI * 0.35, PI * 1.35, 2.0, GREEN);
            let (a, r) = (-PI * 0.35, 5.5);
            let (ex, ey) = (8.0 + r * a.cos(), 8.0 + r * a.sin());
            d.poly(&[(ex - 3.0, ey - 1.0), (ex + 1.6, ey - 3.2), (ex + 1.2, ey + 2.4)], GREEN, GREEN_DARK);
        }
        Icon::Copy => {
            d.page(1.0, 1.0, 10.0, 12.0);
            d.page(6.0, 4.0, 15.0, 15.0);
        }
        Icon::Search => {
            d.line(&[(9.5, 9.5), (14.5, 14.5)], 2.4, GREY_DARK);
            d.circle(6.5, 6.5, 4.6, Color32::from_rgb(0xd8, 0xec, 0xff), BLUE_DARK);
        }
        Icon::Edit => {
            d.poly(&[(3.0, 11.0), (11.0, 3.0), (13.0, 5.0), (5.0, 13.0)], YELLOW, YELLOW_DARK);
            d.poly(&[(3.0, 11.0), (5.0, 13.0), (2.0, 14.0)], SKIN, SKIN_DARK);
            d.poly(&[(11.0, 3.0), (12.0, 2.0), (14.0, 4.0), (13.0, 5.0)], RED, RED);
        }
        Icon::Add => d.plus(8.0, 8.0),
        Icon::Run => d.poly(&[(4.0, 2.5), (13.5, 8.0), (4.0, 13.5)], GREEN, GREEN_DARK),
        Icon::Save => {
            d.rect(1.5, 1.5, 14.5, 14.5, BLUE, BLUE_DARK);
            d.rect(4.0, 1.5, 12.0, 6.5, WHITE, BLUE_DARK);
            d.fill(9.5, 2.5, 11.0, 5.5, GREY_DARK);
            d.rect(3.5, 9.0, 12.5, 14.5, GREY_LIGHT, BLUE_DARK);
        }
        Icon::Design => {
            d.table(1.0, 2.0, 13.0, 13.0);
            d.poly(&[(7.0, 13.0), (13.0, 7.0), (15.0, 9.0), (9.0, 15.0)], YELLOW, YELLOW_DARK);
            d.poly(&[(7.0, 13.0), (9.0, 15.0), (6.0, 16.0)], SKIN, SKIN_DARK);
        }
        Icon::NewTable => {
            d.table(1.0, 1.0, 12.0, 12.0);
            d.plus(12.0, 12.0);
        }
        Icon::Key => {
            d.circle(4.5, 6.5, 3.3, YELLOW, YELLOW_DARK);
            d.circle(4.5, 6.5, 1.0, WHITE, YELLOW_DARK);
            d.line(&[(7.5, 7.5), (14.5, 11.5)], 2.0, YELLOW_DARK);
            d.line(&[(12.0, 10.2), (11.0, 12.2)], 1.6, YELLOW_DARK);
            d.line(&[(14.0, 11.3), (13.2, 13.0)], 1.6, YELLOW_DARK);
        }
        Icon::Up => {
            d.rect(5.5, 8.0, 10.5, 14.5, BLUE, BLUE_DARK);
            d.poly(&[(8.0, 1.5), (14.5, 8.5), (1.5, 8.5)], BLUE, BLUE_DARK);
        }
        Icon::Down => {
            d.rect(5.5, 1.5, 10.5, 8.0, BLUE, BLUE_DARK);
            d.poly(&[(8.0, 14.5), (1.5, 7.5), (14.5, 7.5)], BLUE, BLUE_DARK);
        }
        Icon::AddField => {
            d.table(1.0, 1.0, 13.0, 12.0);
            d.plus(12.0, 12.0);
        }
        Icon::InsertField => {
            d.table(1.0, 1.0, 13.0, 13.0);
            d.fill(1.5, 7.0, 12.5, 9.5, YELLOW);
            d.poly(&[(15.5, 8.2), (12.5, 5.5), (12.5, 11.0)], GREEN, GREEN_DARK);
        }
        Icon::Export => {
            d.page(1.0, 1.0, 11.0, 15.0);
            d.poly(&[(15.5, 10.0), (11.0, 6.0), (11.0, 14.0)], GREEN, GREEN_DARK);
            d.fill(7.0, 8.7, 11.5, 11.3, GREEN);
        }
        Icon::Import => {
            d.page(5.0, 1.0, 15.0, 15.0);
            d.poly(&[(10.5, 10.0), (6.0, 6.0), (6.0, 14.0)], BLUE, BLUE_DARK);
            d.fill(1.0, 8.7, 6.5, 11.3, BLUE);
        }
        Icon::DeleteField => {
            d.table(1.0, 1.0, 13.0, 12.0);
            d.line(&[(9.5, 9.5), (14.5, 14.5)], 2.2, RED);
            d.line(&[(14.5, 9.5), (9.5, 14.5)], 2.2, RED);
        }
    }
}
