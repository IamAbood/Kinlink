//! Interface language: English (built in) or Arabic. `tr("English text")` returns the text in
//! the chosen language, ready to draw (Arabic is put in right-to-left order).
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

static ARABIC: AtomicBool = AtomicBool::new(false);

pub fn set_arabic(on: bool) {
    ARABIC.store(on, Relaxed);
}

pub fn arabic() -> bool {
    ARABIC.load(Relaxed)
}

/// Windows (or the phone) is set to Arabic: the default for new installs.
#[cfg(windows)]
pub fn system_is_arabic() -> bool {
    let lang = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    lang & 0x3FF == 0x01 // LANG_ARABIC
}
#[cfg(not(windows))]
pub fn system_is_arabic() -> bool {
    false
}

/// Translated text, ready to draw.
pub fn tr(en: &str) -> String {
    if !arabic() {
        return en.to_string();
    }
    match crate::i18n_ar::get(en) {
        Some(ar) => crate::rtl::show(ar),
        None => en.to_string(),
    }
}

/// Translated text in typing order, for messages that are stored and drawn later through
/// rtl::show (toasts, bubbles, errors, Windows notifications).
pub fn tl(en: &str) -> String {
    if arabic() { crate::i18n_ar::get(en).unwrap_or(en).to_string() } else { en.to_string() }
}

/// tl with `{}` placeholders filled in order.
pub fn tlf(en: &str, args: &[&str]) -> String {
    let t = if arabic() { crate::i18n_ar::get(en).unwrap_or(en) } else { en };
    let mut out = String::new();
    let mut rest = t;
    for a in args {
        match rest.split_once("{}") {
            Some((head, tail)) => {
                out.push_str(head);
                out.push_str(a);
                rest = tail;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Translated template with `{}` placeholders filled in order, ready to draw.
pub fn trf(en: &str, args: &[&str]) -> String {
    let out = tlf(en, args);
    if arabic() { crate::rtl::show(&out) } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_passthrough_and_templates() {
        set_arabic(false);
        assert_eq!(tr("Friends"), "Friends");
        assert_eq!(trf("Waiting for {} to accept…", &["sami#1"]), "Waiting for sami#1 to accept…");
        set_arabic(true);
        assert_ne!(tr("Friends"), "Friends", "has an Arabic translation");
        assert!(trf("Waiting for {} to accept…", &["sami#1"]).contains("sami#1"));
        assert_eq!(tr("no translation for this"), "no translation for this");
        set_arabic(false);
    }
}
