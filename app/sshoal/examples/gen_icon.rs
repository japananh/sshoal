//! Render `assets/icon.svg` into a macOS `.iconset` directory of PNGs.
//! Used by `scripts/package-macos.sh` to build the app's `.icns`.
//!
//! Usage: `cargo run --release --example gen_icon -p sshoal -- <out.iconset>`

use std::path::Path;

fn main() {
    let svg = include_str!("../assets/icon.svg");
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/sshoal.iconset".to_string());
    std::fs::create_dir_all(&out).expect("create iconset dir");

    let tree =
        resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default()).expect("parse icon svg");

    // The set of sizes macOS `iconutil` expects.
    let specs = [
        ("icon_16x16.png", 16u32),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    ];

    for (name, size) in specs {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("alloc pixmap");
        let scale = size as f32 / 512.0;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        pixmap
            .save_png(Path::new(&out).join(name))
            .expect("save png");
    }

    println!("{out}");
}
