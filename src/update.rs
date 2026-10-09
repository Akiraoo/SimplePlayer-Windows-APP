//! Update check against the GitHub releases of this project (pre-releases count too,
//! since that is how versions are published). Nothing is downloaded: the user gets a link.

use reqwest::blocking::Client;
use serde::Deserialize;
use std::time::Duration;

const API_URL: &str =
    "https://api.github.com/repos/Akiraoo/SimplePlayer-Windows-APP/releases?per_page=10";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
}

pub struct NewVersion {
    pub version: String,
    pub url: String,
}

/// "v0.4.1" / "0.4.1-beta" → [0, 4, 1]
fn parse(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches(['v', 'V'])
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

fn newer(candidate: &str, current: &str) -> bool {
    let (mut a, mut b) = (parse(candidate), parse(current));
    a.resize(3, 0);
    b.resize(3, 0);
    a > b
}

/// Ok(Some(..)) = a newer release exists; Ok(None) = up to date.
pub fn check(client: &Client) -> Result<Option<NewVersion>, String> {
    let r = client
        .get(API_URL)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .send()
        .map_err(|e| crate::tr!("無法連線到 GitHub：{}", "Can't reach GitHub: {}", e))?;
    if !r.status().is_success() {
        return Err(crate::tr!(
            "GitHub 回應 HTTP {}",
            "GitHub replied HTTP {}",
            r.status()
        ));
    }
    let list: Vec<Release> = r
        .json()
        .map_err(|e| crate::tr!("無法讀取版本資訊：{}", "Can't read release info: {}", e))?;
    let current = env!("CARGO_PKG_VERSION");
    // the list is newest first; take the highest version to be safe
    let best = list
        .into_iter()
        .filter(|r| !r.draft)
        .max_by(|a, b| parse(&a.tag_name).cmp(&parse(&b.tag_name)));
    Ok(best
        .filter(|r| newer(&r.tag_name, current))
        .map(|r| NewVersion {
            version: r.tag_name.trim_start_matches(['v', 'V']).to_string(),
            url: r.html_url,
        }))
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    {
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                hwnd: *mut std::ffi::c_void,
                op: *const u16,
                file: *const u16,
                params: *const u16,
                dir: *const u16,
                show: i32,
            ) -> *mut std::ffi::c_void;
        }
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (op, file) = (wide("open"), wide(url));
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // SW_SHOWNORMAL
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}
