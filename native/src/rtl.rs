//! Arabic (and other right-to-left scripts) in egui.
//! egui 0.37 already shapes text with HarfBuzz (harfrust): within one run of Arabic letters it
//! joins them and lays them out right to left. What it doesn't do is order the *runs*: spaces
//! and digits come from another font, so a sentence's words stay left to right. `show` fixes
//! that: it puts each line in visual order (Unicode bidi), then restores the letter order
//! inside each Arabic run, because egui will reverse those itself.
//! Apply only to text being drawn; send/store the original text.
use std::sync::Arc;
use unicode_bidi::BidiInfo;

fn rtl_char(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF)
}

pub fn has_rtl(s: &str) -> bool {
    s.chars().any(rtl_char)
}

/// Text ready to draw (unchanged when it has no right-to-left letters).
pub fn show(s: &str) -> String {
    if !has_rtl(s) {
        return s.to_string();
    }
    s.split('\n').map(line).collect::<Vec<_>>().join("\n")
}

fn line(l: &str) -> String {
    let info = BidiInfo::new(l, None);
    let visual: String = info.paragraphs.iter().map(|p| info.reorder_line(p, p.range.clone()).into_owned()).collect();
    // Undo the reversal inside each run of right-to-left letters (egui's shaper redoes it).
    let mut out = String::with_capacity(visual.len());
    let mut run: Vec<char> = Vec::new();
    for c in visual.chars() {
        if rtl_char(c) {
            run.push(c);
        } else {
            out.extend(run.drain(..).rev());
            out.push(c);
        }
    }
    out.extend(run.drain(..).rev());
    out
}

/// A system font that has Arabic letters (Segoe UI on Windows, Noto on Android).
pub fn arabic_font() -> Option<Vec<u8>> {
    #[cfg(windows)]
    let candidates: Vec<std::path::PathBuf> = {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
        ["segoeui.ttf", "tahoma.ttf", "arial.ttf"].iter().map(|f| dir.join(f)).collect()
    };
    #[cfg(not(windows))]
    let candidates: Vec<std::path::PathBuf> = ["NotoNaskhArabic-Regular.ttf", "NotoNaskhArabicUI-Regular.ttf", "NotoSansArabic-Regular.ttf", "DroidSansArabic.ttf"]
        .iter()
        .map(|f| std::path::Path::new("/system/fonts").join(f))
        .collect();
    let found = candidates.iter().find_map(|p| std::fs::read(p).ok());
    if found.is_none() {
        crate::plog!("fonts: no Arabic-capable system font found");
    }
    found
}

/// Adds the Arabic font as a fallback behind egui's own fonts (phone app).
pub fn add_fonts(ctx: &egui::Context) {
    let Some(bytes) = arabic_font() else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("arabic".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("arabic".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_order_fixed_letters_left_for_shaper() {
        assert_eq!(show("hello"), "hello");
        // Arabic sentence: words swap places (drawn right to left), letters keep typing order.
        assert_eq!(show("حاسوب أحمد"), "أحمد حاسوب");
        // English first: stays left to right, the Arabic part reads right to left.
        assert_eq!(show("kareem: مرحبا كيف"), "kareem: كيف مرحبا");
        // Arabic first with an English word inside.
        assert_eq!(show("افتح PC الآن"), "الآن PC افتح");
    }
}
