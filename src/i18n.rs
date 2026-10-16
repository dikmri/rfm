//! Compile-time bilingual strings. Language selection is fixed before workers start.
use std::sync::{
    LazyLock,
    atomic::{AtomicU8, Ordering},
};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    #[default]
    System = 0,
    Japanese = 1,
    English = 2,
}

impl Language {
    pub fn from_setting(value: &[u16]) -> Self {
        if value.iter().copied().eq("Japanese".encode_utf16()) {
            Self::Japanese
        } else if value.iter().copied().eq("English".encode_utf16()) {
            Self::English
        } else {
            Self::System
        }
    }

    pub fn resolve(self, windows_language: u16) -> Self {
        match self {
            Self::System if windows_language & 0x03ff == 0x11 => Self::Japanese,
            Self::System => Self::English,
            explicit => explicit,
        }
    }
}

static SELECTED: AtomicU8 = AtomicU8::new(Language::System as u8);
static SYSTEM: LazyLock<Language> =
    LazyLock::new(|| Language::System.resolve(unsafe { GetUserDefaultUILanguage() }));

/// Called once after loading preferences, before constructing any app windows.
pub fn initialize(language: Language) {
    SELECTED.store(language as u8, Ordering::Relaxed);
}

pub fn is_japanese() -> bool {
    match SELECTED.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => *SYSTEM == Language::Japanese,
    }
}

pub fn text(japanese: &'static str, english: &'static str) -> &'static str {
    if is_japanese() { japanese } else { english }
}

// Keep the original and translation together. Both branches are compiled and
// format arguments are checked by Rust, with no runtime catalog or substitution.
macro_rules! tr {
    ($japanese:expr, $english:expr $(,)?) => {
        $crate::i18n::text($japanese, $english)
    };
}

macro_rules! tr_w {
    ($japanese:literal, $english:literal $(,)?) => {
        if $crate::i18n::is_japanese() {
            windows_sys::core::w!($japanese)
        } else {
            windows_sys::core::w!($english)
        }
    };
}

macro_rules! tr_format {
    ($japanese:literal, $english:literal $(,)?) => {
        if $crate::i18n::is_japanese() {
            format!($japanese)
        } else {
            format!($english)
        }
    };
    ($japanese:literal, $english:literal, $($args:tt)*) => {
        if $crate::i18n::is_japanese() {
            format!($japanese, $($args)*)
        } else {
            format!($english, $($args)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::Language;

    #[test]
    fn system_language_uses_primary_langid_and_falls_back_to_english() {
        assert_eq!(Language::System.resolve(0x0411), Language::Japanese);
        assert_eq!(Language::System.resolve(0x0811), Language::Japanese);
        for language in [0, 0x0409, 0x0809, 0x0407, 0x0804] {
            assert_eq!(Language::System.resolve(language), Language::English);
        }
    }

    #[test]
    fn explicit_language_overrides_the_windows_display_language() {
        assert_eq!(Language::Japanese.resolve(0x0409), Language::Japanese);
        assert_eq!(Language::English.resolve(0x0411), Language::English);
    }
}
