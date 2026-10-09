//! Interface language: Traditional Chinese (the original text) or English.
//! Rust-side text goes through `tr!("中文 {}", "English {}", args…)`; the Slint side reads
//! the `Lang.en` global, which main.rs keeps in sync with `set_english`.

use std::sync::atomic::{AtomicBool, Ordering};

static EN: AtomicBool = AtomicBool::new(false);

pub fn english() -> bool {
    EN.load(Ordering::Relaxed)
}

pub fn set_english(on: bool) {
    EN.store(on, Ordering::Relaxed);
}

/// Language for a first start: Chinese when Windows is set to Chinese, otherwise English.
pub fn system_is_chinese() -> bool {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
        }
        let mut buf = [0u16; 85];
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n > 1 {
            let name = String::from_utf16_lossy(&buf[..(n - 1) as usize]);
            return name.to_ascii_lowercase().starts_with("zh");
        }
        true
    }
    #[cfg(not(windows))]
    {
        std::env::var("LANG")
            .map(|l| l.to_ascii_lowercase().starts_with("zh"))
            .unwrap_or(true)
    }
}

/// `tr!("中文", "English")` → String; extra arguments are passed to `format!` (use
/// positional `{}` placeholders, the same count in both texts).
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::english() {
            format!($en $(, $arg)*)
        } else {
            format!($zh $(, $arg)*)
        }
    };
}
