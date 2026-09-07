mod autotype;
mod config;
mod ext_connector;
mod fs;
mod hwcrypto;
mod native;
mod net;
mod paths;
mod power;
mod shortcuts;
mod spawn;
mod webkit_prefs;
mod window;

use std::{collections::{HashMap, VecDeque}, path::PathBuf, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Mutex}};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{tray::TrayIcon, AppHandle, Emitter, Manager, RunEvent};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupInfo {
    pub platform: &'static str,
    pub arch: &'static str,
    pub version: &'static str,
    pub open_file: Option<String>,
    pub open_keyfile: Option<String>,
    pub start_minimized: bool,
    pub dev_mode: bool,
}

impl StartupInfo {
    fn from_args(args: &[String]) -> Self {
        Self {
            platform: if cfg!(target_os = "macos") { "darwin" } else if cfg!(target_os = "windows") { "win32" } else { "linux" },
            arch: if cfg!(target_arch = "aarch64") { "arm64" } else if cfg!(target_arch = "x86") { "ia32" } else { "x64" },
            version: env!("CARGO_PKG_VERSION"),
            open_file: args.iter().skip(1).find(|arg| arg.to_ascii_lowercase().ends_with(".kdbx") && !arg.starts_with("--")).cloned(),
            open_keyfile: args.iter().find_map(|arg| arg.strip_prefix("--keyfile=").map(str::to_owned)),
            start_minimized: args.iter().any(|arg| arg.starts_with("--minimized")),
            dev_mode: cfg!(debug_assertions),
        }
    }
}

pub struct Shell {
    pub tray: Mutex<Option<TrayIcon>>,
    pub hook_before_quit: AtomicBool,
    pub exit_requested: AtomicBool,
    pub hidden_in_tray: AtomicBool,
    pub has_open_files: AtomicBool,
    pub teardown_generation: AtomicU64,
    pub window_ready: AtomicBool,
    pub startup_info_read: AtomicBool,
    pub pending_events: Mutex<VecDeque<(String, Value)>>,
    pub watchers: Mutex<HashMap<u32, notify::RecommendedWatcher>>,
    pub startup: StartupInfo,
    pub portable: bool,
    pub user_data_dir: PathBuf,
}

pub fn emit_app_event(app: &AppHandle, name: &str, data: Value) {
    if name != "log" {
        if let Some(shell) = app.try_state::<Shell>() {
            let mut pending = shell.pending_events.lock().unwrap_or_else(|err| err.into_inner());
            if !shell.window_ready.load(Ordering::SeqCst) || app.get_webview_window("main").is_none() {
                if pending.len() == 32 {
                    pending.pop_front();
                }
                pending.push_back((name.to_owned(), data));
                return;
            }
        }
    }
    if let Err(err) = app.emit("app-event", json!({ "name": name, "data": data })) {
        eprintln!("Cannot emit {name}: {err}");
    }
}

#[tauri::command]
fn dev_log(level: String, message: String) {
    eprintln!("[js:{level}] {message}");
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let startup = StartupInfo::from_args(&args);

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            if let Err(err) = window::show_main_window(app.clone()) {
                emit_app_event(app, "log", json!(err));
            }
            let startup = StartupInfo::from_args(&args);
            if let Some(file) = startup.open_file {
                let file = PathBuf::from(&cwd).join(file);
                let key = startup.open_keyfile.map(|key| PathBuf::from(&cwd).join(key));
                emit_app_event(app, "launcher-open-file", json!({ "data": file, "key": key }));
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().with_handler(shortcuts::handle).build())
        .manage(shortcuts::ShortcutActions::default())
        .manage(native::NativeState::default())
        .manage(autotype::AutoTypeState::default())
        .manage(ext_connector::ExtConnectorState::default())
        .setup(move |app| {
            let (user_data_dir, portable) = paths::user_data_dir(app.handle())?;
            let config = config::ConfigStore::new(&user_data_dir, portable)?;
            let settings = config.load(&user_data_dir, "app-settings")?
                .and_then(|text| serde_json::from_str::<Value>(&text).ok()).unwrap_or_else(|| json!({}));
            app.manage(Shell {
                tray: Mutex::new(None),
                hook_before_quit: AtomicBool::new(false),
                exit_requested: AtomicBool::new(false),
                hidden_in_tray: AtomicBool::new(false),
                has_open_files: AtomicBool::new(false),
                teardown_generation: AtomicU64::new(0),
                window_ready: AtomicBool::new(false),
                startup_info_read: AtomicBool::new(false),
                pending_events: Mutex::new(VecDeque::new()),
                watchers: Mutex::new(HashMap::new()),
                startup,
                portable,
                user_data_dir,
            });
            app.manage(config);
            window::setup(app)?;
            let mut configured = HashMap::new();
            for (key, setting) in [
                ("autoType", "globalShortcutAutoType"),
                ("copyPassword", "globalShortcutCopyPassword"),
                ("copyUser", "globalShortcutCopyUser"),
                ("copyUrl", "globalShortcutCopyUrl"),
                ("copyOtp", "globalShortcutCopyOtp"),
                ("restoreApp", "globalShortcutRestoreApp"),
            ] {
                configured.insert(key.to_owned(), settings.get(setting).and_then(Value::as_str).map(str::to_owned));
            }
            if let Err(err) = shortcuts::register(app.handle(), configured) {
                eprintln!("Cannot register all global shortcuts: {err}");
            }
            power::start(app.handle());
            Ok(())
        })
        .on_window_event(window::on_window_event)
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "app-quit" {
                window::request_quit(app);
            }
        })
        .invoke_handler(tauri::generate_handler![
            config::load_config,
            config::save_config,
            paths::get_path,
            fs::fs_read,
            fs::fs_read_text,
            fs::fs_write,
            fs::fs_exists,
            fs::fs_delete,
            fs::fs_stat,
            fs::fs_mkdir,
            fs::fs_read_dir,
            fs::fs_watch_start,
            fs::fs_watch_stop,
            spawn::spawn_process,
            shortcuts::set_global_shortcuts,
            window::minimize_app,
            window::minimize_then_hide_if_in_tray,
            window::show_main_window,
            window::hide_app,
            window::is_app_focused,
            window::set_hook_before_quit,
            window::set_has_open_files,
            window::window_ready,
            window::quit_app,
            window::set_menu_labels,
            window::open_devtools,
            window::resolve_proxy,
            window::get_startup_info,
            dev_log,
            native::argon2,
            native::yubikey_list,
            native::yubikey_challenge_response,
            native::yubikey_cancel_challenge_response,
            native::usb_listener_start,
            native::usb_listener_stop,
            autotype::kbd_get_active_window,
            autotype::kbd_get_active_pid,
            autotype::kbd_show_window,
            autotype::kbd_text,
            autotype::kbd_text_as_keys,
            autotype::kbd_key_press,
            autotype::kbd_shortcut,
            autotype::kbd_key_move_with_modifier,
            autotype::kbd_key_press_with_character,
            autotype::kbd_ensure_modifier_not_pressed,
            hwcrypto::hardware_crypto_delete_key,
            hwcrypto::hardware_encrypt,
            hwcrypto::hardware_decrypt,
            ext_connector::browser_extension_connector_start,
            ext_connector::browser_extension_connector_stop,
            ext_connector::browser_extension_connector_enable,
            ext_connector::browser_extension_connector_socket_result,
            ext_connector::browser_extension_connector_socket_event,
            ext_connector::browser_extension_connector_close_socket,
            net::http_request,
            net::download_to_file,
            net::oauth_listener_start,
            net::oauth_listener_stop,
        ])
        .build(tauri::generate_context!())
        .expect("Cannot initialize KeeWeb")
        .run(|app, event| match event {
            RunEvent::ExitRequested { api, code, .. } => {
                let shell = app.state::<Shell>();
                if !shell.exit_requested.load(Ordering::SeqCst)
                    && code.is_none()
                    && shell.tray.lock().is_ok_and(|tray| tray.is_some())
                {
                    // Destroying the last webview must leave the tray process running.
                    api.prevent_exit();
                } else if shell.hook_before_quit.load(Ordering::SeqCst) && !shell.exit_requested.load(Ordering::SeqCst) {
                    api.prevent_exit();
                    window::request_quit(app);
                } else if app.get_webview_window("main").is_some() {
                    if let Err(err) = window::save_position(app) {
                        eprintln!("Cannot save window position: {err}");
                    }
                }
            }
            RunEvent::Exit => {
                let _ = app.global_shortcut().unregister_all();
            }
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => {
                if let Err(err) = window::show_main_window(app.clone()) {
                    emit_app_event(app, "log", json!(err));
                }
            }
            #[cfg(target_os = "macos")]
            RunEvent::Opened { urls } => {
                for url in urls {
                    if let Ok(path) = url.to_file_path() {
                        if path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("kdbx")) {
                            emit_app_event(app, "launcher-open-file", json!({ "data": path, "key": null }));
                            if let Err(err) = window::show_main_window(app.clone()) {
                                emit_app_event(app, "log", json!(err));
                            }
                        }
                    }
                }
            }
            _ => {}
        });
}
