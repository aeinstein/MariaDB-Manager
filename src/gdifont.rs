//! Schriftdarstellung über Windows-GDI (Prototyp).
//!
//! egui rastert Glyphen selbst (skrifa + vello) und richtet ältere TrueType-Schriften wie Arial
//! dabei nur vertikal am Pixelraster aus – senkrechte Striche liegen oft auf zwei Pixelspalten.
//! Hier übernimmt GDI das Rastern (`GetGlyphOutlineW` mit `GGO_GRAY8_BITMAP`): volles
//! TrueType-Hinting wie im Graustufenmodus von Windows, dazu die gerundeten Vorschübe von GDI.
//! Eingehängt als `GlyphRasterizer` mit höchster Priorität, nur für die Proportionalschrift;
//! alles, was die Schrift nicht kennt (Icons, Sonderzeichen), fällt auf egui zurück.

use eframe::egui::{
    self,
    epaint::text::{FontPriority, GlyphBitmap, GlyphRasterizer, GlyphRasterizerRequest, RasterizedGlyph},
    Color32, ColorImage,
};
use std::collections::HashMap;
use std::sync::Mutex;
use windows_sys::Win32::Graphics::Gdi::*;

/// Speicher-DC mit einer Schrift je Pixelgröße. GDI-Handles sind rohe Zeiger, daher als `isize`.
struct Gdi {
    face: Vec<u16>,
    /// Schriftstärke 0..=1: hebt die Kanten an (Gamma) und verbreitert Striche (siehe `embolden`).
    weight: f32,
    dc: isize,
    fonts: HashMap<i32, isize>,
}

impl Gdi {
    /// Schrift der Größe `px` (Em-Höhe) in den DC wählen.
    fn select(&mut self, px: i32) -> HDC {
        let dc = self.dc as HDC;
        let face = &self.face;
        let font = *self.fonts.entry(px).or_insert_with(|| unsafe {
            CreateFontW(
                -px, // negativ = Em-Höhe statt Zellenhöhe, wie egui
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_TT_ONLY_PRECIS as u32,
                0,
                ANTIALIASED_QUALITY as u32,
                0,
                face.as_ptr(),
            ) as isize
        });
        unsafe { SelectObject(dc, font as HGDIOBJ) };
        dc
    }

    fn rasterize(&mut self, req: &GlyphRasterizerRequest<'_>) -> Option<RasterizedGlyph> {
        let mut chars = req.cluster.chars();
        let ch = chars.next()?;
        // Nur einzelne Zeichen der Grundebene; Kombinationen überlässt man egui.
        if chars.next().is_some() || ch as u32 > 0xFFFF || ch.is_control() {
            return None;
        }
        let px = req.font_size_px.round() as i32;
        if !(4..=200).contains(&px) {
            return None;
        }
        let dc = self.select(px);
        let w = [ch as u16];
        let mut gi = 0u16;
        if unsafe { GetGlyphIndicesW(dc, w.as_ptr(), 1, &mut gi, GGI_MARK_NONEXISTING_GLYPHS) } == u32::MAX || gi == 0xFFFF {
            return None;
        }
        let one = FIXED { fract: 0, value: 1 };
        let zero = FIXED { fract: 0, value: 0 };
        let mat = MAT2 { eM11: one, eM12: zero, eM21: zero, eM22: one };
        let mut gm: GLYPHMETRICS = unsafe { std::mem::zeroed() };
        let size = unsafe { GetGlyphOutlineW(dc, ch as u32, GGO_GRAY8_BITMAP, &mut gm, 0, std::ptr::null_mut(), &mat) };
        if size == GDI_ERROR as u32 {
            return None;
        }
        let advance_px = gm.gmCellIncX as f32;
        let (bw, bh) = (gm.gmBlackBoxX as usize, gm.gmBlackBoxY as usize);
        // Leerzeichen u. Ä.: unsichtbares 1×1-Bild, damit trotzdem der GDI-Vorschub gilt
        // (ein leeres Bild würde egui verwerfen und auf die eigene Schrift zurückfallen).
        if size == 0 || bw == 0 || bh == 0 {
            return Some(RasterizedGlyph {
                bitmap: GlyphBitmap { image: ColorImage::new([1, 1], vec![Color32::TRANSPARENT]), offset_px: egui::Vec2::ZERO, is_color: false },
                advance_px,
            });
        }
        let mut buf = vec![0u8; size as usize];
        let r = unsafe { GetGlyphOutlineW(dc, ch as u32, GGO_GRAY8_BITMAP, &mut gm, size, buf.as_mut_ptr().cast(), &mat) };
        if r == GDI_ERROR as u32 {
            return None;
        }
        // Zeilen sind auf 4 Byte ausgerichtet; Werte 0..=64
        let stride = (bw + 3) & !3;
        if buf.len() < stride * bh {
            return None;
        }
        let cov: Vec<f32> = (0..bh).flat_map(|y| buf[y * stride..y * stride + bw].iter()).map(|&v| v.min(64) as f32 / 64.0).collect();
        let (cov, bw) = embolden(&cov, bw, bh, self.weight);
        let pixels = cov.into_iter().map(|a| Color32::from_white_alpha((a * 255.0).round() as u8)).collect();
        Some(RasterizedGlyph {
            bitmap: GlyphBitmap {
                image: ColorImage::new([bw, bh], pixels),
                // Ursprung = Grundlinie; GDI zählt y nach oben, egui nach unten
                offset_px: egui::vec2(gm.gmptGlyphOrigin.x as f32, -gm.gmptGlyphOrigin.y as f32),
                is_color: false,
            },
            advance_px,
        })
    }
}

/// Macht die Glyphe kräftiger. Bis `weight` 0,5 nur über Gamma (Kantenpixel dunkler),
/// darüber zusätzlich horizontal verbreitert: Jedes Pixel bekommt einen Anteil seines linken
/// Nachbarn – senkrechte Striche werden um bis zu ½ Pixel breiter, Bild um 1 Spalte breiter.
fn embolden(cov: &[f32], w: usize, h: usize, weight: f32) -> (Vec<f32>, usize) {
    let weight = weight.clamp(0.0, 1.0);
    let gamma = 1.0 - 0.8 * weight.min(0.5); // 1,0 … 0,6
    let smear = ((weight - 0.5).max(0.0) * 2.0) * 0.5; // 0 … 0,5
    let w2 = if smear > 0.0 { w + 1 } else { w };
    let mut out = vec![0.0f32; w2 * h];
    for y in 0..h {
        for x in 0..w2 {
            let cur = if x < w { cov[y * w + x] } else { 0.0 };
            let left = if x > 0 && x - 1 < w { cov[y * w + x - 1] } else { 0.0 };
            let a = cur.max(left * smear).min(1.0);
            out[y * w2 + x] = a.powf(gamma);
        }
    }
    (out, w2)
}

impl Drop for Gdi {
    fn drop(&mut self) {
        unsafe {
            for f in self.fonts.values() {
                DeleteObject(*f as HGDIOBJ);
            }
            DeleteDC(self.dc as HDC);
        }
    }
}

/// GDI-Rasterung für die Proportionalschrift einschalten (`face` = GDI-Schriftname, z. B. „Arial“,
/// `weight` = Schriftstärke 0..=1) oder mit `None` wieder abschalten.
pub fn install(ctx: &egui::Context, face: Option<&str>, weight: f32) {
    let Some(face) = face else {
        ctx.set_glyph_rasterizers(Vec::new());
        return;
    };
    let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
    if dc.is_null() {
        ctx.set_glyph_rasterizers(Vec::new());
        return;
    }
    let gdi = Mutex::new(Gdi { face: face.encode_utf16().chain([0]).collect(), weight, dc: dc as isize, fonts: HashMap::new() });
    let r = GlyphRasterizer::new(format!("mariadb_manager::gdifont::{face}::{weight}"), move |req| {
        if *req.family != egui::FontFamily::Proportional {
            return None;
        }
        gdi.lock().ok()?.rasterize(req)
    })
    .with_priority(FontPriority::Highest);
    ctx.set_glyph_rasterizers(vec![r]);
}
