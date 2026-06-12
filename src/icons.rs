use anyhow::{Context, Result};
use tao::window::Theme;

pub struct IconColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub fn popup_icon_color(muted: bool, theme: Theme) -> IconColor {
    // Matches the tray-icon convention: red flags a hot (unmuted) mic;
    // neutral foreground (black/white per theme) means muted.
    match theme {
        Theme::Light if !muted => IconColor {
            r: 239,
            g: 68,
            b: 68,
        }, // #ef4444
        Theme::Light => IconColor { r: 0, g: 0, b: 0 },
        Theme::Dark if !muted => IconColor {
            r: 248,
            g: 113,
            b: 113,
        }, // #f87171
        _ => IconColor {
            r: 255,
            g: 255,
            b: 255,
        },
    }
}

pub fn tray_icon_color(muted: bool) -> IconColor {
    if muted {
        // Drawn black; the tray icon is set as a macOS template image, so
        // the system tints it (black on a light menu bar, white on a dark
        // one) — same behavior as built-in system icons.
        IconColor { r: 0, g: 0, b: 0 }
    } else {
        // #ef4444 — red, used to flag that the mic is hot. Not a template,
        // so the color is preserved regardless of menu-bar appearance.
        IconColor {
            r: 239,
            g: 68,
            b: 68,
        }
    }
}

/// Rasterizes an SVG with the given stroke color.
/// Returns un-premultiplied RGBA bytes plus the source dimensions.
pub fn rasterize_svg(svg_bytes: &[u8], color: &IconColor) -> Result<(Vec<u8>, u32, u32)> {
    let svg_str = std::str::from_utf8(svg_bytes).context("SVG is not valid UTF-8")?;
    let colored = svg_str.replacen(
        "<svg ",
        &format!("<svg stroke=\"rgb({},{},{})\" ", color.r, color.g, color.b),
        1,
    );

    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(&colored, &options).context("Failed to parse SVG")?;
    let size = tree.size();
    let w = size.width() as u32;
    let h = size.height() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("Failed to allocate pixmap")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );

    // tiny-skia produces premultiplied RGBA; un-premultiply for callers.
    let raw = pixmap.take();
    let straight: Vec<u8> = raw
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3];
            if a == 0 {
                [0u8, 0, 0, 0]
            } else {
                let s = 255.0_f32 / a as f32;
                [
                    (p[0] as f32 * s).min(255.) as u8,
                    (p[1] as f32 * s).min(255.) as u8,
                    (p[2] as f32 * s).min(255.) as u8,
                    a,
                ]
            }
        })
        .collect();

    Ok((straight, w, h))
}
