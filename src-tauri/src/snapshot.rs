//! `dev_snapshot`: renders the webview to a PNG file. Debug tooling for visual checks
//! (there is no CDP/screenshot API in wry; macOS screen capture needs TCC consent).

use tauri::WebviewWindow;

fn enabled() -> bool {
    cfg!(debug_assertions) || std::env::var_os("KEEWEB_STARTUP_LOGGING").is_some()
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn dev_snapshot(window: WebviewWindow, path: String) -> Result<(), String> {
    use block2::RcBlock;
    use objc2::{msg_send, rc::Retained, runtime::AnyObject};
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_foundation::{NSData, NSDictionary, NSError};
    use std::sync::mpsc;

    if !enabled() {
        return Err("dev_snapshot is only available in debug builds".into());
    }
    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, String>>();
    window
        .with_webview(move |webview| unsafe {
            let wk: &AnyObject = &*(webview.inner().cast::<AnyObject>());
            let tx = tx.clone();
            let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                let result = if image.is_null() {
                    let message = if error.is_null() {
                        "snapshot failed".to_owned()
                    } else {
                        (*error).localizedDescription().to_string()
                    };
                    Err(message)
                } else {
                    let image = &*image;
                    let tiff: Option<Retained<NSData>> = image.TIFFRepresentation();
                    match tiff.and_then(|tiff| NSBitmapImageRep::imageRepWithData(&tiff)) {
                        Some(rep) => {
                            let png: Option<Retained<NSData>> = rep
                                .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new());
                            png.map(|png| png.to_vec()).ok_or_else(|| "PNG encoding failed".to_owned())
                        }
                        None => Err("cannot decode snapshot".to_owned()),
                    }
                };
                let _ = tx.send(result);
            });
            let _: () = msg_send![wk, takeSnapshotWithConfiguration: std::ptr::null::<AnyObject>(), completionHandler: &*handler];
        })
        .map_err(|err| err.to_string())?;
    let bytes = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "snapshot timed out".to_owned())??;
    std::fs::write(&path, bytes).map_err(|err| err.to_string())
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn dev_snapshot(_window: WebviewWindow, _path: String) -> Result<(), String> {
    Err("dev_snapshot is only implemented on macOS".into())
}
