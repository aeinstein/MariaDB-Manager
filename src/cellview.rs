//! Detailbereich unter dem Grid: zeigt mehrzeilige/lange Zellinhalte vollständig an,
//! binäre Inhalte als Bild (falls dekodierbar) bzw. als Hex-Dump.

use eframe::egui::{self, RichText};

/// Ab dieser Länge wird Text auch einzeilig im Detailbereich gezeigt (Grid kürzt).
const LONG_TEXT: usize = 256;
/// Größere Binärinhalte werden nicht vollständig nachgeladen.
pub const MAX_FETCH: usize = 32 * 1024 * 1024;
/// Hex-Dump höchstens für so viele Bytes.
const MAX_DUMP: usize = 64 * 1024;

/// Zelle: (Ergebnis-Nr., Zeile, Spalte).
pub type CellKey = (usize, usize, usize);

enum Content {
    None,
    Text(String),
    Binary {
        bytes: Vec<u8>,
        /// Gesamtgröße, wenn `bytes` gekürzt ist.
        total: Option<usize>,
        loading: bool,
        image: Option<Result<(egui::TextureHandle, String), String>>,
        dump: String,
    },
}

pub struct CellView {
    key: Option<CellKey>,
    hidden: Option<CellKey>,
    col: String,
    content: Content,
    error: Option<String>,
}

impl Default for CellView {
    fn default() -> Self {
        Self { key: None, hidden: None, col: String::new(), content: Content::None, error: None }
    }
}

impl CellView {
    /// Übernimmt die aktuelle Grid-Auswahl. Liefert `true`, wenn ein gekürzter Binärwert
    /// vollständig nachgeladen werden soll (nur bei `can_fetch`).
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        key: Option<CellKey>,
        value: Option<&str>,
        binary: bool,
        col: &str,
        can_fetch: bool,
    ) -> bool {
        if key == self.key {
            // Text kann sich durch Bearbeiten ändern
            if let (Content::Text(t), Some(v)) = (&mut self.content, value) {
                if t != v {
                    *t = v.to_string();
                }
            }
            return false;
        }
        self.key = key;
        self.col = col.to_string();
        self.error = None;
        self.content = Content::None;
        let Some(v) = value else { return false };
        if binary {
            let (bytes, total) = parse_hex(v);
            let truncated = total.is_some_and(|t| t > bytes.len());
            self.set_binary(ctx, bytes, if truncated { total } else { None });
            if truncated {
                let t = total.unwrap_or(0);
                if can_fetch && t <= MAX_FETCH {
                    if let Content::Binary { loading, .. } = &mut self.content {
                        *loading = true;
                    }
                    return true;
                }
            }
        } else if v.contains(['\n', '\r']) || v.chars().count() > LONG_TEXT {
            self.content = Content::Text(v.to_string());
        }
        false
    }

    /// Vollständig nachgeladener Binärinhalt.
    pub fn set_full(&mut self, ctx: &egui::Context, key: CellKey, bytes: Vec<u8>) {
        if self.key == Some(key) {
            self.set_binary(ctx, bytes, None);
        }
    }

    pub fn set_error(&mut self, key: CellKey, e: String) {
        if self.key == Some(key) {
            self.error = Some(e);
            if let Content::Binary { loading, .. } = &mut self.content {
                *loading = false;
            }
        }
    }

    fn set_binary(&mut self, ctx: &egui::Context, bytes: Vec<u8>, total: Option<usize>) {
        let image = if total.is_none() { decode_image(ctx, &bytes) } else { None };
        let dump = if matches!(image, Some(Ok(_))) { String::new() } else { hex_dump(&bytes) };
        self.content = Content::Binary { bytes, total, loading: false, image, dump };
    }

    /// Zeichnet den Bereich als unteres Panel; vor dem Grid aufrufen.
    pub fn ui(&mut self, ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug) {
        if matches!(self.content, Content::None) || (self.hidden.is_some() && self.hidden == self.key) {
            return;
        }
        let mut close = false;
        egui::Panel::bottom(egui::Id::unique(("cellview", id)))
            .resizable(true)
            .default_size(200.0)
            .min_size(80.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&self.col);
                    let info = match &self.content {
                        Content::Text(t) => format!("{} Zeichen, {} Zeilen", t.chars().count(), t.lines().count().max(1)),
                        Content::Binary { bytes, total, image, .. } => {
                            let size = fmt_bytes(total.unwrap_or(bytes.len()));
                            match image {
                                Some(Ok((_, fmt))) => format!("{fmt}, {size}"),
                                _ => format!("Binär, {size}"),
                            }
                        }
                        Content::None => String::new(),
                    };
                    ui.weak(info);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✖").on_hover_text("Bereich ausblenden").clicked() {
                            close = true;
                        }
                        if let Content::Text(t) = &self.content {
                            if ui.small_button("Kopieren").clicked() {
                                ui.ctx().copy_text(t.clone());
                            }
                        }
                    });
                });
                if let Some(e) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                match &self.content {
                    Content::Text(t) => {
                        egui::ScrollArea::both().id_salt("cv_text").auto_shrink([false, false]).show(ui, |ui| {
                            ui.add(egui::TextEdit::multiline(&mut t.as_str()).code_editor().desired_width(f32::INFINITY));
                        });
                    }
                    Content::Binary { bytes, total, loading, image, dump } => {
                        if *loading {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Lade vollständigen Inhalt …");
                            });
                        } else if let Some(t) = total {
                            ui.weak(format!(
                                "Inhalt gekürzt: {} von {} geladen – nachladbar nur bis {} und nur für Spalten aus einer Tabelle.",
                                fmt_bytes(bytes.len()),
                                fmt_bytes(*t),
                                fmt_bytes(MAX_FETCH)
                            ));
                        }
                        match image {
                            Some(Ok((tex, _))) => {
                                egui::ScrollArea::both().id_salt("cv_img").auto_shrink([false, false]).show(ui, |ui| {
                                    let avail = ui.available_size();
                                    let size = tex.size_vec2();
                                    // Nur verkleinern, nie vergrößern
                                    let scale = (avail.x / size.x).min(avail.y / size.y).min(1.0).max(0.05);
                                    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(tex.id(), size * scale)));
                                });
                            }
                            _ => {
                                if !*loading {
                                    if let Some(Err(e)) = image {
                                        ui.weak(format!("Kein Bild erkannt ({e}) – Hex-Ansicht:"));
                                    }
                                    egui::ScrollArea::both().id_salt("cv_hex").auto_shrink([false, false]).show(ui, |ui| {
                                        ui.add(egui::Label::new(RichText::new(dump.as_str()).monospace()).wrap_mode(egui::TextWrapMode::Extend));
                                    });
                                }
                            }
                        }
                    }
                    Content::None => {}
                }
            });
        if close {
            self.hidden = self.key;
        }
    }
}

/// `0xABCD…` (ggf. mit `… (N Bytes)`) → (Bytes, Gesamtgröße falls gekürzt).
pub fn parse_hex(v: &str) -> (Vec<u8>, Option<usize>) {
    let s = v.strip_prefix("0x").unwrap_or(v);
    let (hex, rest) = match s.find('…') {
        Some(p) => (&s[..p], Some(&s[p..])),
        None => (s, None),
    };
    let bytes = decode_hex(hex);
    let total = rest.and_then(|r| r.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().ok());
    (bytes, total)
}

pub fn decode_hex(hex: &str) -> Vec<u8> {
    let b = hex.trim().as_bytes();
    let val = |c: u8| match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    };
    b.chunks_exact(2).map(|p| val(p[0]) << 4 | val(p[1])).collect()
}

fn decode_image(ctx: &egui::Context, bytes: &[u8]) -> Option<Result<(egui::TextureHandle, String), String>> {
    if bytes.is_empty() {
        return None;
    }
    let fmt = match image::guess_format(bytes) {
        Ok(f) => f,
        Err(_) => return Some(Err("unbekanntes Format".into())),
    };
    Some(match image::load_from_memory_with_format(bytes, fmt) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let ci = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
            let tex = ctx.load_texture("cellview_image", ci, egui::TextureOptions::LINEAR);
            Ok((tex, format!("{}-Bild {w}×{h}", format!("{fmt:?}").to_uppercase())))
        }
        Err(e) => Err(e.to_string()),
    })
}

fn hex_dump(bytes: &[u8]) -> String {
    let n = bytes.len().min(MAX_DUMP);
    let mut s = String::with_capacity(n * 4 + 64);
    for (i, chunk) in bytes[..n].chunks(16).enumerate() {
        s.push_str(&format!("{:08X}  ", i * 16));
        for k in 0..16 {
            match chunk.get(k) {
                Some(b) => s.push_str(&format!("{b:02X} ")),
                None => s.push_str("   "),
            }
            if k == 7 {
                s.push(' ');
            }
        }
        s.push(' ');
        s.extend(chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }));
        s.push('\n');
    }
    if n < bytes.len() {
        s.push_str(&format!("… ({} weitere Bytes)\n", bytes.len() - n));
    }
    s
}

fn fmt_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} Bytes")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("0xDEADBEEF00FF"), (vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0xff], None));
        assert_eq!(parse_hex("0x0102… (5000 Bytes)"), (vec![1, 2], Some(5000)));
    }

    #[test]
    fn png_wird_erkannt() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let ctx = egui::Context::default();
        match decode_image(&ctx, &png) {
            Some(Ok((tex, info))) => {
                assert_eq!(tex.size(), [3, 2]);
                assert_eq!(info, "PNG-Bild 3×2");
            }
            other => panic!("kein Bild: {:?}", other.map(|r| r.map(|(_, i)| i))),
        }
        assert!(matches!(decode_image(&ctx, b"kein bild"), Some(Err(_))));
    }
}
