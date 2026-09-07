use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::time::Duration;

use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Manager, State};
use zeroize::Zeroizing;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use challenge_response::{config::{Config, Mode, Slot}, error::ChallengeResponseError, ChallengeResponse};
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use futures_util::StreamExt;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use nusb::MaybeFuture;

const YUBICO_VENDOR_ID: u16 = 0x1050;
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct NativeState {
    usb_listener: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    // A device exchange must finish before listing or starting another one: serial reads
    // use the same HID command channel and would otherwise corrupt the pending response.
    challenge: Mutex<Option<Arc<PendingChallenge>>>,
}

struct PendingChallenge {
    callback_id: u32,
    finished: AtomicBool,
}

impl PendingChallenge {
    fn finish(&self, app: &AppHandle, result: Result<Vec<u8>, Value>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let (error, result) = match result {
            Ok(bytes) => (None, Some(bytes)),
            Err(error) => (Some(error), None),
        };
        crate::emit_app_event(app, "native-modules-yubikey-chalresp-result", json!({
            "callbackId": self.callback_id, "error": error, "result": result
        }));
    }
}

#[derive(Deserialize)]
pub struct Argon2Options {
    #[serde(rename = "type")]
    algorithm: u32,
    version: u32,
    memory: u32,
    iterations: u32,
    parallelism: u32,
    length: usize,
}

#[tauri::command]
pub async fn argon2(password: Vec<u8>, salt: Vec<u8>, options: Argon2Options) -> Result<tauri::ipc::Response, String> {
    let password = Zeroizing::new(password);
    let salt = Zeroizing::new(salt);
    tauri::async_runtime::spawn_blocking(move || {
        let algorithm = match options.algorithm {
            0 => Algorithm::Argon2d,
            1 => Algorithm::Argon2i,
            2 => Algorithm::Argon2id,
            _ => return Err("Invalid Argon2 type".to_owned()),
        };
        let version = Version::try_from(options.version).map_err(|err| err.to_string())?;
        // Both kdbxweb and RustCrypto express memory in KiB; do not multiply by 1024.
        let params = Params::new(options.memory, options.iterations, options.parallelism, Some(options.length))
            .map_err(|err| err.to_string())?;
        let mut hash = Vec::new();
        hash.try_reserve_exact(options.length).map_err(|err| err.to_string())?;
        hash.resize(options.length, 0);
        Argon2::new(algorithm, version, params)
            .hash_password_into(&password, &salt, &mut hash)
            .map_err(|err| err.to_string())?;
        Ok(tauri::ipc::Response::new(hash))
    }).await.map_err(|err| err.to_string())?
}

#[derive(Deserialize)]
pub struct YubiKeySelector {
    serial: Option<u32>,
    vid: Option<u16>,
    pid: Option<u16>,
}

#[derive(Serialize)]
pub struct YubiKeyInfo {
    serial: u32,
    vid: u16,
    pid: u16,
    version: String,
    slots: [YubiKeySlot; 2],
}

#[derive(Serialize)]
struct YubiKeySlot {
    number: u8,
    valid: bool,
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn challenge_error(error: ChallengeResponseError) -> Value {
    let message = error.to_string();
    let code = match &error {
        ChallengeResponseError::DeviceNotFound
        | ChallengeResponseError::UsbError(rusb::Error::NoDevice) => "YK_ENOKEY",
        ChallengeResponseError::UsbError(rusb::Error::Timeout) => "YK_ETIMEOUT",
        ChallengeResponseError::IOError(err) if err.kind() == std::io::ErrorKind::TimedOut => "YK_ETIMEOUT",
        _ => &message,
    };
    json!({ "code": code, "message": message })
}

#[tauri::command]
pub async fn yubikey_list(app: AppHandle, config: Map<String, Value>) -> Result<Vec<YubiKeyInfo>, String> {
    let _ = config; // KeeWeb currently passes {}; there are no enumeration filters in use.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    return tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<NativeState>();
        let pending = state.challenge.try_lock().map_err(|_| "YK_EBUSY: YubiKey is in use")?;
        if pending.is_some() {
            return Err("YK_EBUSY: YubiKey is in use".to_owned());
        }
        let mut client = ChallengeResponse::new().map_err(|err| err.to_string())?;
        let devices = match client.find_all_devices() {
            Ok(devices) => devices,
            Err(ChallengeResponseError::DeviceNotFound) => return Ok(Vec::new()),
            Err(err) => return Err(err.to_string()),
        };
        let usb_devices = rusb::devices().map_err(|err| err.to_string())?;
        let mut keys = Vec::new();
        for device in devices.into_iter().filter(|device| device.vendor_id == YUBICO_VENDOR_ID) {
            let Some(usb_device) = usb_devices.iter().find(|usb_device| {
                usb_device.bus_number() == device.bus_id && usb_device.address() == device.address_id
            }) else {
                continue; // The key was unplugged during enumeration.
            };
            let descriptor = usb_device.device_descriptor().map_err(|err| err.to_string())?;
            keys.push(YubiKeyInfo {
                serial: device.serial.unwrap_or(0),
                vid: device.vendor_id,
                pid: device.product_id,
                version: descriptor.device_version().to_string(),
                // ponytail: the crate cannot query slot configuration; attempting an
                // unconfigured slot reports a terminal error instead of hiding the slot.
                slots: [YubiKeySlot { number: 1, valid: true }, YubiKeySlot { number: 2, valid: true }],
            });
        }
        Ok(keys)
    }).await.map_err(|err| err.to_string())?;
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Err("Not supported on this platform".to_owned())
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn perform_challenge(app: &AppHandle, pending: &PendingChallenge, yubikey: YubiKeySelector, challenge: &[u8], slot: Slot) -> Result<Vec<u8>, Value> {
    if pending.finished.load(Ordering::Acquire) {
        return Err(json!({ "code": "YK_ECANCELED", "message": "YubiKey challenge canceled" }));
    }
    let mut client = ChallengeResponse::new().map_err(challenge_error)?;
    let device = match yubikey.serial {
        Some(serial) => client.find_device_from_serial(serial),
        None => client.find_device(),
    }.map_err(challenge_error)?;
    if device.vendor_id != YUBICO_VENDOR_ID
        || yubikey.vid.is_some_and(|vid| vid != device.vendor_id)
        || yubikey.pid.is_some_and(|pid| pid != device.product_id)
    {
        return Err(challenge_error(ChallengeResponseError::DeviceNotFound));
    }
    if pending.finished.load(Ordering::Acquire) {
        return Err(json!({ "code": "YK_ECANCELED", "message": "YubiKey challenge canceled" }));
    }
    // The crate exposes neither touch configuration nor intermediate HID status.
    // Prompt before entering its blocking call, including for keys that need no touch.
    crate::emit_app_event(app, "native-modules-yubikey-chalresp-result", json!({
        "callbackId": pending.callback_id,
        "error": { "code": "YK_ETOUCH", "touchRequested": true, "message": "Touch your YubiKey if it is blinking" },
        "result": null
    }));
    let config = Config::new_from(device).set_variable_size(true).set_mode(Mode::Sha1).set_slot(slot);
    client.challenge_response_hmac(challenge, config)
        .map(|result| result.to_vec()).map_err(challenge_error)
}

#[tauri::command]
pub async fn yubikey_challenge_response(app: AppHandle, state: State<'_, NativeState>, yubikey: YubiKeySelector, challenge: Vec<u8>, slot: u8, callback_id: u32) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let slot = match slot {
            1 => Slot::Slot1,
            2 => Slot::Slot2,
            _ => return Err("Invalid YubiKey slot: expected 1 or 2".to_owned()),
        };
        if challenge.len() > 64 {
            return Err("YubiKey challenges must not exceed 64 bytes".to_owned());
        }
        let challenge = Zeroizing::new(challenge);
        let mut active = state.challenge.try_lock().map_err(|_| "YK_EBUSY: YubiKey is in use")?;
        if active.is_some() {
            return Err("YK_EBUSY: Previous YubiKey operation is still finishing; reconnect the key if it is stuck".to_owned());
        }
        let pending = Arc::new(PendingChallenge { callback_id, finished: AtomicBool::new(false) });
        *active = Some(pending.clone());
        let started = std::thread::Builder::new().name("yubikey-challenge".to_owned()).spawn(move || {
            let timeout_app = app.clone();
            let timeout_pending = pending.clone();
            let timeout = tauri::async_runtime::spawn(async move {
                tokio::time::sleep(CHALLENGE_TIMEOUT).await;
                timeout_pending.finish(&timeout_app, Err(json!({
                    "code": "YK_ETIMEOUT", "message": "Timed out waiting for the YubiKey"
                })));
            });
            let result = perform_challenge(&app, &pending, yubikey, &challenge, slot);
            timeout.abort();
            // Clear the hardware reservation before publishing the result so a callback
            // can immediately begin the next exchange (KDBX may request several).
            let state = app.state::<NativeState>();
            if let Ok(mut active) = state.challenge.lock() {
                *active = None;
            }
            pending.finish(&app, result);
        });
        if let Err(err) = started {
            *active = None;
            return Err(err.to_string());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Err("Not supported on this platform".to_owned())
}

#[tauri::command]
pub async fn yubikey_cancel_challenge_response(app: AppHandle, state: State<'_, NativeState>) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let active = state.challenge.lock().map_err(|err| err.to_string())?;
        if let Some(pending) = active.as_ref() {
            // Best effort: the crate polls HID synchronously with no cancellation API.
            // Set the flag and settle JS now; discard late results. Keep the reservation
            // until HID returns to avoid overlapping exchanges. Unplug a stuck key.
            pending.finish(&app, Err(json!({ "code": "YK_ECANCELED", "message": "YubiKey challenge canceled" })));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Err("Not supported on this platform".to_owned())
}

#[tauri::command]
pub async fn usb_listener_start(app: AppHandle) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    return tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<NativeState>();
        let mut listener = state.usb_listener.lock().map_err(|err| err.to_string())?;
        if listener.is_some() {
            return Ok(());
        }
        // Subscribe before enumerating so no attach/detach can fall in between.
        let mut watch = nusb::watch_devices().map_err(|err| err.to_string())?;
        let mut attached: std::collections::HashSet<_> = nusb::list_devices().wait()
            .map_err(|err| err.to_string())?
            .filter(|device| device.vendor_id() == YUBICO_VENDOR_ID)
            .map(|device| device.id()).collect();
        crate::emit_app_event(&app, "native-modules-yubikeys", json!(attached.len()));
        let watch_app = app.clone();
        *listener = Some(tauri::async_runtime::spawn(async move {
            while let Some(event) = watch.next().await {
                let changed = match event {
                    nusb::hotplug::HotplugEvent::Connected(device) => {
                        device.vendor_id() == YUBICO_VENDOR_ID && attached.insert(device.id())
                    }
                    nusb::hotplug::HotplugEvent::Disconnected(id) => attached.remove(&id),
                };
                if changed {
                    crate::emit_app_event(&watch_app, "native-modules-yubikeys", json!(attached.len()));
                }
            }
        }));
        Ok(())
    }).await.map_err(|err| err.to_string())?;
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Err("Not supported on this platform".to_owned())
}

#[tauri::command]
pub async fn usb_listener_stop(state: State<'_, NativeState>) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        let mut listener = state.usb_listener.lock().map_err(|err| err.to_string())?;
        if let Some(listener) = listener.take() {
            listener.abort();
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Err("Not supported on this platform".to_owned())
}
