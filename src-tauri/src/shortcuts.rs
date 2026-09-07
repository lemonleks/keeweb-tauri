use std::{collections::HashMap, str::FromStr, sync::Mutex};

use serde_json::Value;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

use crate::{emit_app_event, window};

#[derive(Default)]
pub struct ShortcutActions(Mutex<HashMap<u32, &'static str>>);

fn normalize(accelerator: &str) -> String {
    accelerator.split('+').map(|token| match token.trim() {
        "Ctrl" | "Control" => "Control",
        "Cmd" | "Command" => "Super",
        "CmdOrCtrl" | "CommandOrControl" => if cfg!(target_os = "macos") { "Super" } else { "Control" },
        "Option" | "Alt" => "Alt",
        key => key,
    }).collect::<Vec<_>>().join("+")
}

pub fn handle(app: &AppHandle, shortcut: &Shortcut, event: ShortcutEvent) {
    if event.state != ShortcutState::Pressed {
        return;
    }
    let action = app.state::<ShortcutActions>().0.lock().ok().and_then(|actions| actions.get(&shortcut.id()).copied());
    if let Some(action) = action {
        if action == "restore-app" {
            if let Err(err) = window::show_main_window(app.clone()) {
                emit_app_event(app, "log", Value::String(err));
            }
        } else {
            emit_app_event(app, action, Value::Null);
            if action == "auto-type" && app.get_webview_window("main").is_none() {
                if let Err(err) = window::show_main_window(app.clone()) {
                    emit_app_event(app, "log", Value::String(err));
                }
            }
        }
    }
}

pub fn register(app: &AppHandle, shortcuts: HashMap<String, Option<String>>) -> Result<(), String> {
    app.global_shortcut().unregister_all().map_err(|err| err.to_string())?;
    app.state::<ShortcutActions>().0.lock().map_err(|err| err.to_string())?.clear();
    let modifiers = if cfg!(target_os = "macos") { "Ctrl+Alt+" } else { "Shift+Alt+" };
    let mut errors = Vec::new();
    for (key, suffix, action) in [
        ("autoType", "T", "auto-type"),
        ("copyPassword", "C", "copy-password"),
        ("copyUser", "B", "copy-user"),
        ("copyUrl", "U", "copy-url"),
        ("copyOtp", "", "copy-otp"),
        ("restoreApp", "", "restore-app"),
    ] {
        let accelerator = shortcuts.get(key).and_then(|value| value.as_deref()).filter(|value| !value.is_empty())
            .map(str::to_owned).unwrap_or_else(|| if suffix.is_empty() { String::new() } else { format!("{modifiers}{suffix}") });
        if accelerator.is_empty() {
            continue;
        }
        let result = Shortcut::from_str(&normalize(&accelerator)).map_err(|err| err.to_string()).and_then(|shortcut| {
            app.global_shortcut().register(shortcut).map_err(|err| err.to_string())?;
            app.state::<ShortcutActions>().0.lock().map_err(|err| err.to_string())?.insert(shortcut.id(), action);
            Ok(())
        });
        if let Err(err) = result {
            errors.push(format!("{key} ({accelerator}): {err}"));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}

#[tauri::command]
pub async fn set_global_shortcuts(app: AppHandle, shortcuts: HashMap<String, Option<String>>) -> Result<(), String> {
    register(&app, shortcuts)
}
