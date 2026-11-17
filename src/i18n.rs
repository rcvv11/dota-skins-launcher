//! Interface language: Russian or English. The setting (`config.json`
//! `language`) is "ru", "en" or empty for automatic: Russian when Windows'
//! interface language is Russian, English otherwise.
//!
//! Strings carry both texts at the call site:
//!   `tr!("Текст", "Text")` for a plain string,
//!   `trf!("Найдено: {n}", "Found: {n}", ...)` for a `format!` template
//! (both templates take the same arguments, so inline `{name}` works in both).

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

/// Whether Windows' interface language is Russian (primary language 0x19).
pub fn system_is_russian() -> bool {
    (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 0x19
}

/// Applies a language setting: "ru", "en" or "" (automatic).
pub fn set(setting: &str) {
    let english = match setting.trim() {
        "en" => true,
        "ru" => false,
        _ => !system_is_russian(),
    };
    ENGLISH.store(english, Ordering::Relaxed);
}

pub fn is_en() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Code of the language in effect ("ru" / "en"), for the DLL.
pub fn code() -> &'static str {
    if is_en() { "en" } else { "ru" }
}

/// A plain string in the current language.
#[macro_export]
macro_rules! tr {
    ($ru:literal, $en:literal) => {
        if $crate::i18n::is_en() { $en } else { $ru }
    };
}

/// A `format!` in the current language.
#[macro_export]
macro_rules! trf {
    ($ru:literal, $en:literal $($rest:tt)*) => {
        if $crate::i18n::is_en() { format!($en $($rest)*) } else { format!($ru $($rest)*) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn macros_pick_the_language() {
        let n = 3;
        super::set("en");
        assert_eq!(tr!("Да", "Yes"), "Yes");
        assert_eq!(trf!("Найдено: {n}", "Found: {n}"), "Found: 3");
        assert_eq!(trf!("{} из {}", "{} of {}", 1, 2), "1 of 2");
        super::set("ru");
        assert_eq!(tr!("Да", "Yes"), "Да");
        assert_eq!(trf!("Найдено: {n}", "Found: {n}"), "Найдено: 3");
    }
}
