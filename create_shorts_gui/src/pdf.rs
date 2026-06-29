//! Export a one-page PDF listing the most-recently created shorts, with
//! **clickable** YouTube links. Shared by the GUI button and the
//! `--export-pdf` CLI flag so both produce an identical document.
//!
//! Links are real PDF URI link annotations (not just blue text), so a
//! click in any PDF viewer opens the video. We use `printpdf`'s built-in
//! Helvetica fonts (no font files to ship) and lay text out top-down in
//! millimetres, converting to printpdf's bottom-left origin as we go.

use crate::history::UploadEntry;
use printpdf::{
    Actions, BorderArray, BuiltinFont, Color, ColorArray, HighlightingMode, LinkAnnotation, Mm,
    PdfDocument, Rect, Rgb,
};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

const PAGE_W: f32 = 210.0; // A4 width  (mm)
const PAGE_H: f32 = 297.0; // A4 height (mm)
const MARGIN: f32 = 18.0; // left/right/top/bottom margin (mm)

/// 1 pt in mm — printpdf font sizes are in pt, our layout is in mm.
const PT_MM: f32 = 0.352_777_8;

/// Default location for the exported PDF: the user's Desktop if we can
/// find it, otherwise the app config dir. Filename encodes the count.
pub fn default_output_path(n: usize) -> PathBuf {
    let name = format!("latest_{}_shorts.pdf", n);
    if let Some(desktop) = dirs::desktop_dir() {
        if desktop.is_dir() {
            return desktop.join(name);
        }
    }
    crate::settings::config_dir().join(name)
}

/// Render up to `max` newest entries to a PDF at `out`. Returns the number
/// of shorts actually written. Entries with an empty URL are skipped.
pub fn export(entries: &[UploadEntry], max: usize, out: &Path) -> Result<usize, String> {
    let shorts: Vec<&UploadEntry> = entries
        .iter()
        .filter(|e| !e.url.trim().is_empty())
        .take(max)
        .collect();

    let (doc, page1, layer1) =
        PdfDocument::new("Latest Shorts by J\u{00fc}rg Da Vaz", Mm(PAGE_W), Mm(PAGE_H), "Layer 1");
    let font = doc
        .add_builtin_font(BuiltinFont::Helvetica)
        .map_err(|e| format!("font: {e}"))?;
    let font_bold = doc
        .add_builtin_font(BuiltinFont::HelveticaBold)
        .map_err(|e| format!("font: {e}"))?;
    let layer = doc.get_page(page1).get_layer(layer1);

    // y is a top-down cursor in mm from the top of the page; `at` converts
    // it to printpdf's bottom-left origin for a baseline.
    let at = |y: f32| Mm(PAGE_H - y);
    let mut y = MARGIN;

    // ---- header ----
    // Title: "Latest Shorts by Jürg Da Vaz" with the name in blue and
    // linked to davaz.com.
    let title_size = 22.0;
    let title_baseline = y + 7.0;
    let prefix = "Latest Shorts by ";
    let name = "J\u{00fc}rg Da Vaz";
    layer.use_text(prefix, title_size, Mm(MARGIN), at(title_baseline), &font_bold);
    let name_x = MARGIN + approx_width_mm(prefix, title_size);
    layer.set_fill_color(Color::Rgb(Rgb::new(0.0, 0.0, 0.8, None)));
    layer.use_text(name, title_size, Mm(name_x), at(title_baseline), &font_bold);
    layer.set_fill_color(Color::Rgb(Rgb::new(0.0, 0.0, 0.0, None)));
    add_link_box(
        &layer,
        "https://davaz.com",
        name_x,
        title_baseline,
        approx_width_mm(name, title_size),
        title_size,
    );
    y += 10.0;
    let subtitle = format!(
        "{} short{} \u{2014} create_shorts",
        shorts.len(),
        if shorts.len() == 1 { "" } else { "s" }
    );
    layer.use_text(&subtitle, 10.0, Mm(MARGIN), at(y + 4.0), &font);
    y += 10.0;
    // divider
    layer.use_text(
        "\u{2014}".repeat(60),
        8.0,
        Mm(MARGIN),
        at(y),
        &font,
    );
    y += 6.0;

    if shorts.is_empty() {
        layer.use_text(
            "No shorts found in upload history yet.",
            12.0,
            Mm(MARGIN),
            at(y + 6.0),
            &font,
        );
    }

    // ---- one block per short ----
    for (i, e) in shorts.iter().enumerate() {
        // crude page-overflow guard: stop before running off the bottom.
        if y > PAGE_H - MARGIN - 24.0 {
            layer.use_text(
                format!("\u{2026} and {} more", shorts.len() - i),
                10.0,
                Mm(MARGIN),
                at(y + 5.0),
                &font,
            );
            break;
        }

        let title = if e.title.trim().is_empty() {
            "(untitled)".to_string()
        } else {
            e.title.trim().to_string()
        };
        let line1 = format!("{}. {}", i + 1, title);
        layer.use_text(&line1, 13.0, Mm(MARGIN), at(y + 5.0), &font_bold);
        y += 8.0;

        // clickable URL line — draw the text in link-blue, then reset to
        // black so the metadata/title that follow stay black.
        let url = e.url.trim();
        let link_x = MARGIN + 4.0;
        let link_size = 11.0;
        layer.set_fill_color(Color::Rgb(Rgb::new(0.0, 0.0, 0.8, None)));
        layer.use_text(url, link_size, Mm(link_x), at(y + 4.0), &font);
        layer.set_fill_color(Color::Rgb(Rgb::new(0.0, 0.0, 0.0, None)));
        add_link_box(&layer, url, link_x, y + 4.0, approx_width_mm(url, link_size), link_size);
        y += 6.5;

        // metadata line (range / privacy / date) — plain text
        let mut meta: Vec<String> = Vec::new();
        if !e.start.trim().is_empty() || !e.end.trim().is_empty() {
            meta.push(format!("{}\u{2013}{}", e.start.trim(), e.end.trim()));
        }
        if !e.privacy.trim().is_empty() {
            meta.push(e.privacy.trim().to_string());
        }
        if !e.timestamp.trim().is_empty() {
            meta.push(e.timestamp.trim().to_string());
        }
        if !meta.is_empty() {
            layer.use_text(
                meta.join("   \u{00b7}   "),
                9.0,
                Mm(link_x),
                at(y + 3.5),
                &font,
            );
            y += 6.0;
        }
        y += 4.0; // gap between blocks
    }

    let file = std::fs::File::create(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    doc.save(&mut BufWriter::new(file))
        .map_err(|e| format!("save: {e}"))?;
    Ok(shorts.len())
}

/// Rough text width in mm from Helvetica's ~0.5em average advance — close
/// enough to size a click box / place a following run on the same line.
fn approx_width_mm(text: &str, size_pt: f32) -> f32 {
    text.chars().count() as f32 * size_pt * 0.5 * PT_MM
}

/// Place an invisible-border URI link annotation of `width_mm` over text
/// whose baseline sits at top-down `baseline_top` mm. The box brackets the
/// baseline (a little below for descenders, most of the cap-height above).
fn add_link_box(
    layer: &printpdf::PdfLayerReference,
    url: &str,
    x: f32,
    baseline_top: f32,
    width_mm: f32,
    size_pt: f32,
) {
    let baseline_y = PAGE_H - baseline_top;
    let ll_y = baseline_y - size_pt * PT_MM * 0.25;
    let ur_y = baseline_y + size_pt * PT_MM * 0.85;
    let rect = Rect::new(Mm(x), Mm(ll_y), Mm(x + width_mm), Mm(ur_y));
    // Zero-width border + transparent color = no visible box, just a
    // clickable region over the blue URL text.
    let annot = LinkAnnotation::new(
        rect,
        Some(BorderArray::Solid([0.0, 0.0, 0.0])),
        Some(ColorArray::Transparent),
        Actions::uri(url.to_string()),
        Some(HighlightingMode::Invert),
    );
    layer.add_link_annotation(annot);
}
