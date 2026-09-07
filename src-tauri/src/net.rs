//! lemonkee is offline by design: no HTTP client is compiled into the binary.
//! The commands stay registered so the web app gets a clear error instead of a
//! missing-command failure. Other layers: CSP in `app/index.html`, the WebKit
//! content-rule blocklist in `webkit_prefs.rs`, and `thirdPartyStoragesSupported: false`
//! in `launcher-tauri.js`.

const DISABLED: &str = "ENETDOWN: network access is disabled in lemonkee";

#[tauri::command]
pub fn http_request(_config: serde_json::Value) -> Result<(), String> {
    Err(DISABLED.into())
}

#[tauri::command]
pub fn download_to_file(_url: String, _path: String) -> Result<(), String> {
    Err(DISABLED.into())
}

#[tauri::command]
pub fn oauth_listener_start(_port: u16, _path: String) -> Result<(), String> {
    Err(DISABLED.into())
}

#[tauri::command]
pub fn oauth_listener_stop() -> Result<(), String> {
    Ok(())
}
