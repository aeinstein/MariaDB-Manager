//! Rastert `assets/icon.svg` in die Icon-Dateien der App:
//! `icon.png` (256 px, Fenstericon), `icon.ico` (Windows-EXE) und `icon.icns` (macOS-Bundle).
//!
//! Aufruf aus dem Projektordner: `cargo run --manifest-path tools/gen-icon/Cargo.toml`

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::ExtendedColorType;
use resvg::{tiny_skia, usvg};
use std::path::PathBuf;

/// SVG in `size`×`size` Pixel rastern (RGBA, nicht vormultipliziert).
fn render(tree: &usvg::Tree, size: u32) -> Vec<u8> {
    let mut pm = tiny_skia::Pixmap::new(size, size).expect("Pixmap");
    let s = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(tree, tiny_skia::Transform::from_scale(s, s), &mut pm.as_mut());
    pm.pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

fn png(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::ImageEncoder::write_image(image::codecs::png::PngEncoder::new(&mut out), rgba, size, size, ExtendedColorType::Rgba8)
        .expect("PNG");
    out
}

fn main() {
    let assets = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    let svg = std::fs::read(assets.join("icon.svg")).expect("assets/icon.svg fehlt");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("SVG ungültig");

    // Fenstericon
    std::fs::write(assets.join("icon.png"), png(&render(&tree, 256), 256)).expect("icon.png");

    // Windows: mehrere Größen in einer ICO-Datei
    let frames: Vec<IcoFrame> = [16, 24, 32, 48, 64, 128, 256]
        .into_iter()
        .map(|s| IcoFrame::as_png(&render(&tree, s), s, s, ExtendedColorType::Rgba8).expect("ICO-Frame"))
        .collect();
    let f = std::fs::File::create(assets.join("icon.ico")).expect("icon.ico");
    IcoEncoder::new(std::io::BufWriter::new(f)).encode_images(&frames).expect("ICO");

    // macOS
    let mut fam = icns::IconFamily::new();
    for s in [16, 32, 64, 128, 256, 512, 1024] {
        let img = icns::Image::from_data(icns::PixelFormat::RGBA, s, s, render(&tree, s)).expect("ICNS-Bild");
        fam.add_icon(&img).expect("ICNS");
    }
    let f = std::fs::File::create(assets.join("icon.icns")).expect("icon.icns");
    fam.write(std::io::BufWriter::new(f)).expect("ICNS schreiben");

    println!("Icons in {} erzeugt.", assets.display());
}
