use std::sync::{Arc, Mutex};

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use serde::{Deserialize, Serialize};
use tauri::State;

// Initialize lazily: launching KeeWeb must not require Accessibility permission
// or an available Linux input backend. Keep the connection across IPC calls.
#[derive(Default)]
pub struct AutoTypeState {
    keyboard: Arc<Mutex<Option<Enigo>>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveWindowOptions {
    #[serde(default)]
    get_window_title: bool,
    #[serde(default)]
    get_browser_url: bool,
}

#[derive(Serialize)]
pub struct ActiveWindow {
    id: String,
    pid: u64,
    title: Option<String>,
    url: Option<String>,
}

async fn keyboard_action(
    state: &AutoTypeState,
    permission_required: bool,
    action: impl FnOnce(&mut Enigo) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let keyboard = Arc::clone(&state.keyboard);
    tauri::async_runtime::spawn_blocking(move || {
        let mut keyboard = keyboard.lock().map_err(|_| "Auto-type keyboard lock poisoned")?;
        #[cfg(target_os = "macos")]
        if !macos::accessibility_permission(permission_required) {
            // Releasing modifiers is best-effort and must not prompt or prevent
            // auto-type from reaching the command that explains the permission.
            return if permission_required {
                Err("Accessibility permission required".into())
            } else {
                Ok(())
            };
        }
        #[cfg(not(target_os = "macos"))]
        let _ = permission_required;
        if keyboard.is_none() {
            *keyboard = Some(Enigo::new(&Settings::default()).map_err(|err| err.to_string())?);
        }
        action(keyboard.as_mut().ok_or("Auto-type keyboard unavailable")?)
    })
    .await
    .map_err(|err| err.to_string())?
}

fn modifiers(names: &[String]) -> Result<Vec<Key>, String> {
    let mut keys = Vec::with_capacity(names.len());
    for name in names {
        let key = match name.as_str() {
            "Ctrl" | "Control" => Key::Control,
            "Alt" | "Option" => Key::Alt,
            "Shift" => Key::Shift,
            "Meta" | "Command" | "Cmd" | "Super" | "Win" | "Windows" => Key::Meta,
            _ => return Err(format!("Bad modifier: {name}")),
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

fn release_keys(keyboard: &mut Enigo, keys: &[Key]) -> Result<(), String> {
    let mut result = Ok(());
    for key in keys.iter().rev() {
        if let Err(err) = keyboard.key(*key, Direction::Release) {
            if result.is_ok() {
                result = Err(err.to_string());
            }
        }
    }
    result
}

fn with_modifiers(
    keyboard: &mut Enigo,
    keys: &[Key],
    action: impl FnOnce(&mut Enigo) -> Result<(), String>,
) -> Result<(), String> {
    for (index, key) in keys.iter().enumerate() {
        if let Err(err) = keyboard.key(*key, Direction::Press) {
            let _ = release_keys(keyboard, &keys[..=index]);
            return Err(err.to_string());
        }
    }
    let result = action(keyboard);
    // Never strand a modifier when text entry or a key press fails.
    let released = release_keys(keyboard, keys);
    result.and(released)
}

fn key_code(code: &str) -> Result<Key, String> {
    let key = match code {
        "Tab" | "tab" => Key::Tab,
        "Return" | "enter" => Key::Return,
        "Space" | "space" => Key::Space,
        "UpArrow" | "up" => Key::UpArrow,
        "DownArrow" | "down" => Key::DownArrow,
        "LeftArrow" | "left" => Key::LeftArrow,
        "RightArrow" | "right" => Key::RightArrow,
        "Home" | "home" => Key::Home,
        "End" | "end" => Key::End,
        "PageUp" | "pgup" => Key::PageUp,
        "PageDown" | "pgdn" => Key::PageDown,
        "ForwardDelete" | "del" => Key::Delete,
        "BackwardDelete" | "bs" => Key::Backspace,
        "Escape" | "esc" => Key::Escape,
        "Meta" | "win" => Key::Meta,
        "F1" | "f1" => Key::F1,
        "F2" | "f2" => Key::F2,
        "F3" | "f3" => Key::F3,
        "F4" | "f4" => Key::F4,
        "F5" | "f5" => Key::F5,
        "F6" | "f6" => Key::F6,
        "F7" | "f7" => Key::F7,
        "F8" | "f8" => Key::F8,
        "F9" | "f9" => Key::F9,
        "F10" | "f10" => Key::F10,
        "F11" | "f11" => Key::F11,
        "F12" | "f12" => Key::F12,
        "F13" | "f13" => Key::F13,
        "F14" | "f14" => Key::F14,
        "F15" | "f15" => Key::F15,
        "F16" | "f16" => Key::F16,
        // Key::Other is a virtual keycode on macOS/Windows and a keysym on X11.
        #[cfg(target_os = "macos")]
        "Insert" | "ins" => Key::Other(0x72), // The Mac Help/Insert key.
        #[cfg(not(target_os = "macos"))]
        "Insert" | "ins" => Key::Insert,
        #[cfg(target_os = "macos")]
        "RightMeta" | "rwin" => Key::RCommand,
        #[cfg(target_os = "windows")]
        "RightMeta" | "rwin" => Key::RWin,
        #[cfg(target_os = "linux")]
        "RightMeta" | "rwin" => Key::Other(0xffec), // XK_Super_R.
        "KeypadPlus" => keypad_key(0x45, 0x6b, 0xffab),
        "KeypadMinus" => keypad_key(0x4e, 0x6d, 0xffad),
        "KeypadMultiply" => keypad_key(0x43, 0x6a, 0xffaa),
        "KeypadDivide" => keypad_key(0x4b, 0x6f, 0xffaf),
        "D0" => Key::Unicode('0'),
        "D1" => Key::Unicode('1'),
        "D2" => Key::Unicode('2'),
        "D3" => Key::Unicode('3'),
        "D4" => Key::Unicode('4'),
        "D5" => Key::Unicode('5'),
        "D6" => Key::Unicode('6'),
        "D7" => Key::Unicode('7'),
        "D8" => Key::Unicode('8'),
        "D9" => Key::Unicode('9'),
        _ => return Err(format!("Bad code: {code}")),
    };
    Ok(key)
}

fn keypad_key(mac: u32, windows: u32, linux: u32) -> Key {
    Key::Other(if cfg!(target_os = "macos") {
        mac
    } else if cfg!(target_os = "windows") {
        windows
    } else {
        linux
    })
}

fn single_character(text: &str) -> Result<char, String> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(character), None) if character != '\0' => Ok(character),
        _ => Err("Expected one non-NUL character".into()),
    }
}

#[tauri::command]
pub async fn kbd_text(text: String, state: State<'_, AutoTypeState>) -> Result<(), String> {
    if text.contains('\0') {
        return Err("Text must not contain NUL characters".into());
    }
    keyboard_action(&state, true, move |keyboard| {
        keyboard.text(&text).map_err(|err| err.to_string())
    }).await
}

#[tauri::command]
pub async fn kbd_text_as_keys(
    text: String,
    modifiers: Vec<String>,
    state: State<'_, AutoTypeState>,
) -> Result<(), String> {
    if text.contains('\0') {
        return Err("Text must not contain NUL characters".into());
    }
    let keys = self::modifiers(&modifiers)?;
    keyboard_action(&state, true, move |keyboard| {
        with_modifiers(keyboard, &keys, |keyboard| {
            for character in text.chars() {
                keyboard.key(Key::Unicode(character), Direction::Click).map_err(|err| err.to_string())?;
            }
            Ok(())
        })
    }).await
}

#[tauri::command]
pub async fn kbd_key_press(
    code: String,
    modifiers: Vec<String>,
    state: State<'_, AutoTypeState>,
) -> Result<(), String> {
    let key = key_code(&code)?;
    let keys = self::modifiers(&modifiers)?;
    keyboard_action(&state, true, move |keyboard| {
        with_modifiers(keyboard, &keys, |keyboard| {
            keyboard.key(key, Direction::Click).map_err(|err| err.to_string())
        })
    }).await
}

#[tauri::command]
pub async fn kbd_shortcut(code: String, state: State<'_, AutoTypeState>) -> Result<(), String> {
    // KeeWeb sends uppercase "V" for paste; do not turn that into Shift+V.
    let key = Key::Unicode(single_character(&code)?.to_ascii_lowercase());
    let modifier = if cfg!(target_os = "macos") { Key::Meta } else { Key::Control };
    keyboard_action(&state, true, move |keyboard| {
        with_modifiers(keyboard, &[modifier], |keyboard| {
            keyboard.key(key, Direction::Click).map_err(|err| err.to_string())
        })
    }).await
}

#[tauri::command]
pub async fn kbd_key_move_with_modifier(
    down: bool,
    modifiers: Vec<String>,
    state: State<'_, AutoTypeState>,
) -> Result<(), String> {
    let keys = self::modifiers(&modifiers)?;
    keyboard_action(&state, down, move |keyboard| {
        if !down {
            return release_keys(keyboard, &keys);
        }
        for (index, key) in keys.iter().enumerate() {
            if let Err(err) = keyboard.key(*key, Direction::Press) {
                let _ = release_keys(keyboard, &keys[..=index]);
                return Err(err.to_string());
            }
        }
        Ok(())
    }).await
}

#[tauri::command]
pub async fn kbd_key_press_with_character(
    character: String,
    code: Option<u32>,
    modifiers: Vec<String>,
    state: State<'_, AutoTypeState>,
) -> Result<(), String> {
    let key = if character.is_empty() {
        let code = code.ok_or("A virtual key code is required when character is empty")?;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if code > u16::MAX as u32 {
            return Err("Virtual key code must fit in 16 bits".into());
        }
        Key::Other(code)
    } else {
        Key::Unicode(single_character(&character)?)
    };
    let keys = self::modifiers(&modifiers)?;
    keyboard_action(&state, true, move |keyboard| {
        with_modifiers(keyboard, &keys, |keyboard| {
            keyboard.key(key, Direction::Click).map_err(|err| err.to_string())
        })
    }).await
}

#[tauri::command]
pub async fn kbd_ensure_modifier_not_pressed(state: State<'_, AutoTypeState>) -> Result<(), String> {
    keyboard_action(&state, false, |keyboard| {
        let _ = release_keys(keyboard, &[Key::Shift, Key::Control, Key::Alt, Key::Meta]);
        // Right-side modifiers are distinct physical keys as well.
        let _ = release_keys(keyboard, &[Key::RShift, Key::RControl]);
        #[cfg(target_os = "macos")]
        let _ = release_keys(keyboard, &[Key::ROption, Key::RCommand]);
        #[cfg(target_os = "windows")]
        let _ = release_keys(keyboard, &[Key::RMenu, Key::RWin]);
        #[cfg(target_os = "linux")]
        let _ = release_keys(keyboard, &[Key::Other(0xffea), Key::Other(0xffec)]);
        Ok(())
    }).await
}

#[tauri::command]
pub async fn kbd_get_active_window(options: ActiveWindowOptions) -> Result<ActiveWindow, String> {
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        let window = objc2::rc::autoreleasepool(|_| active_win_pos_rs::get_active_window());
        #[cfg(not(target_os = "macos"))]
        let window = active_win_pos_rs::get_active_window();
        let window = window.map_err(|_| "Cannot determine the active window")?;
        #[cfg(target_os = "macos")]
        let url = if options.get_browser_url { macos::browser_url(&window.app_name) } else { None };
        #[cfg(not(target_os = "macos"))]
        let url = { let _ = options.get_browser_url; None };
        let title = options.get_window_title.then(|| {
            #[cfg(target_os = "macos")]
            if window.title.is_empty() {
                // CGWindowName is redacted without Screen Recording permission;
                // Accessibility already grants the focused window's title.
                return macos::window_title(window.process_id).unwrap_or(window.title);
            }
            window.title
        });
        Ok(ActiveWindow {
            id: if window.window_id.is_empty() { window.process_id.to_string() } else { window.window_id },
            pid: window.process_id,
            title,
            url,
        })
    }).await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn kbd_get_active_pid() -> Result<u64, String> {
    tauri::async_runtime::spawn_blocking(|| {
        #[cfg(target_os = "macos")]
        return objc2::rc::autoreleasepool(|_| {
            let app = objc2_app_kit::NSWorkspace::sharedWorkspace().frontmostApplication()
                .ok_or("Cannot determine the active process")?;
            u64::try_from(app.processIdentifier()).map_err(|_| "Invalid active process ID".into())
        });
        #[cfg(not(target_os = "macos"))]
        active_win_pos_rs::get_active_window().map(|window| window.process_id)
            .map_err(|_| "Cannot determine the active process".into())
    }).await.map_err(|err| err.to_string())?
}

#[tauri::command]
pub async fn kbd_show_window(id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        return macos::show_window(&id);
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::SetForegroundWindow};
            // active-win-pos-rs 0.11 returns the windows 0.48 HWND debug representation.
            let id = id.strip_prefix("HWND(").and_then(|id| id.strip_suffix(')')).unwrap_or(&id);
            let handle = id.parse::<isize>().map_err(|_| "Invalid window ID")?;
            if handle == 0 {
                return Err("Invalid window ID".into());
            }
            return if unsafe { SetForegroundWindow(HWND(handle as *mut _)) }.as_bool() {
                Ok(())
            } else {
                Err("Cannot activate the requested window".into())
            };
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        { let _ = id; Err("Not supported on this platform".into()) }
    }).await.map_err(|err| err.to_string())?
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        io::Read,
        process::{Command, Stdio},
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::{Duration, Instant},
    };
    use core_foundation::{
        array::{CFArray, CFArrayRef},
        base::{CFType, CFTypeRef, TCFType},
        boolean::CFBoolean,
        dictionary::{CFDictionary, CFDictionaryRef},
        number::CFNumber,
        string::{CFString, CFStringRef},
    };
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
        fn AXUIElementCopyAttributeValue(element: CFTypeRef, name: CFStringRef, value: *mut CFTypeRef) -> i32;
        fn AXUIElementSetMessagingTimeout(element: CFTypeRef, timeout: f32) -> i32;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
        fn CGWindowListCopyWindowInfo(options: u32, relative_to_window: u32) -> CFArrayRef;
    }

    pub fn accessibility_permission(prompt: bool) -> bool {
        static PROMPTED: AtomicBool = AtomicBool::new(false);
        if unsafe { AXIsProcessTrusted() } {
            return true;
        }
        if prompt && !PROMPTED.swap(true, Ordering::Relaxed) {
            let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
            let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
            unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) };
        }
        false
    }

    pub fn window_title(pid: u64) -> Option<String> {
        if !accessibility_permission(false) {
            return None;
        }
        let element = unsafe { AXUIElementCreateApplication(i32::try_from(pid).ok()?) };
        if element.is_null() { return None; }
        let app = unsafe { CFType::wrap_under_create_rule(element) };
        unsafe { AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), 0.5) };
        let attribute = |element: &CFType, name: &str| {
            let name = CFString::new(name);
            let mut value = std::ptr::null();
            let status = unsafe { AXUIElementCopyAttributeValue(element.as_CFTypeRef(), name.as_concrete_TypeRef(), &mut value) };
            if status == 0 && !value.is_null() {
                Some(unsafe { CFType::wrap_under_create_rule(value) })
            } else {
                None
            }
        };
        let window = attribute(&app, "AXFocusedWindow")?;
        attribute(&window, "AXTitle")?.downcast::<CFString>().map(|title| title.to_string())
    }

    pub fn show_window(id: &str) -> Result<(), String> {
        let id = id.parse::<u32>().map_err(|_| "Invalid window ID")?;
        if id == 0 {
            return Err("Invalid window ID".into());
        }
        // Resolve the CGWindowID returned by active-win-pos-rs, not a stale PID cache.
        let windows = unsafe { CGWindowListCopyWindowInfo(1 << 3, id) };
        if windows.is_null() {
            return Err("Cannot find the requested window".into());
        }
        let windows = unsafe { CFArray::<CFDictionary<CFString, CFType>>::wrap_under_create_rule(windows) };
        let window = windows.get(0).ok_or("Cannot find the requested window")?;
        let pid = window.find(CFString::new("kCGWindowOwnerPID"))
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|value| value.to_i64())
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or("Cannot determine the window's process")?;
        objc2::rc::autoreleasepool(|_| {
            let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .ok_or("The requested application is no longer running")?;
            #[allow(deprecated)]
            let activated = app.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
            if activated { Ok(()) } else { Err("Cannot activate the requested application".into()) }
        })
    }

    pub fn browser_url(app: &str) -> Option<String> {
        // Only constant, allowlisted application names enter AppleScript. Never
        // interpolate a window title, URL, or caller-supplied script fragment.
        let script = match app {
            "Safari" => "tell application \"Safari\" to get URL of front document",
            "Google Chrome" => "tell application \"Google Chrome\" to get URL of active tab of front window",
            "Microsoft Edge" => "tell application \"Microsoft Edge\" to get URL of active tab of front window",
            "Brave Browser" => "tell application \"Brave Browser\" to get URL of active tab of front window",
            "Arc" => "tell application \"Arc\" to get URL of active tab of front window",
            _ => return None,
        };
        browser_script_result(script)
    }

    fn browser_script_result(script: &str) -> Option<String> {
        // An Automation permission dialog must not stall the IPC indefinitely.
        let mut child = Command::new("/usr/bin/osascript").args(["-e", script])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        let reader = thread::spawn(move || {
            let mut text = String::new();
            stdout.read_to_string(&mut text).map(|_| text)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let success = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };
        let output = reader.join().ok()?.ok()?;
        if !success { return None; }
        let url = output.trim();
        if url.is_empty() || url == "missing value" { None } else { Some(url.to_owned()) }
    }
}
