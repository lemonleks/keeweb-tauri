use std::{sync::{atomic::{AtomicBool, Ordering}, mpsc, Mutex}, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{image::Image, menu::{Menu, MenuItem}, tray::TrayIconBuilder, AppHandle, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::{config::ConfigStore, emit_app_event, Shell, StartupInfo};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct WindowPosition {
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    maximized: bool,
    full_screen: bool,
}

#[derive(Default)]
struct WindowFlags {
    minimized: bool,
    maximized: bool,
    fullscreen: bool,
}

struct WindowState {
    position: Mutex<WindowPosition>,
    flags: Mutex<WindowFlags>,
    save_signal: mpsc::Sender<()>,
    creating: AtomicBool,
    destroying: AtomicBool,
}

#[derive(Clone, Deserialize)]
pub struct TrayLabels {
    restore: String,
    quit: String,
}

impl Default for TrayLabels {
    fn default() -> Self {
        Self { restore: "Restore".into(), quit: "Quit".into() }
    }
}

fn main_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window("main").ok_or_else(|| "Main window is unavailable".into())
}

fn background_color(settings: &Value, dark: bool) -> tauri::window::Color {
    let mut theme = match settings.get("theme").and_then(Value::as_str).unwrap_or("dark") {
        "macdark" => "dark",
        "wh" => "light",
        theme => theme,
    };
    if settings.get("autoSwitchTheme").and_then(Value::as_bool).unwrap_or(false) {
        for (dark_theme, light_theme) in [("dark", "light"), ("sd", "sl"), ("fb", "bl"), ("db", "lb"), ("te", "lt"), ("dc", "hc")] {
            if theme == dark_theme || theme == light_theme {
                theme = if dark { dark_theme } else { light_theme };
                break;
            }
        }
    }
    let (red, green, blue) = match theme {
        "dark" => (0x1e, 0x1e, 0x1e),
        "light" => (0xf6, 0xf6, 0xf6),
        "db" => (0x34, 0x2f, 0x2e),
        "wh" | "hc" => (0xfa, 0xfa, 0xfa),
        "te" => (0x22, 0x22, 0x22),
        "sd" => (0x00, 0x2b, 0x36),
        "sl" => (0xfd, 0xf6, 0xe3),
        _ => (0x28, 0x2c, 0x34),
    };
    tauri::window::Color(red, green, blue, 255)
}

pub fn setup(app: &mut tauri::App) -> Result<(), String> {
    let (save_signal, save_receiver) = mpsc::channel();
    app.manage(WindowState {
        position: Mutex::new(WindowPosition::default()),
        flags: Mutex::new(WindowFlags::default()),
        save_signal,
        creating: AtomicBool::new(false),
        destroying: AtomicBool::new(false),
    });
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        while save_receiver.recv().is_ok() {
            loop {
                match save_receiver.recv_timeout(Duration::from_millis(500)) {
                    Ok(()) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Err(err) = save_position(&handle) {
                            emit_app_event(&handle, "log", json!(err));
                        }
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });
    let window = create_main_window(app.handle())?;
    if app.state::<Shell>().startup.start_minimized {
        minimize_app(app.handle().clone(), TrayLabels::default())?;
        emit_app_event(app.handle(), "launcher-started-minimized", Value::Null);
    } else {
        show_window(app.handle(), &window)?;
    }
    Ok(())
}

pub fn create_main_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    let shell = app.state::<Shell>();
    let config = app.state::<ConfigStore>();
    let settings = config.load(&shell.user_data_dir, "app-settings")?
        .and_then(|text| serde_json::from_str::<Value>(&text).ok()).unwrap_or_else(|| json!({}));
    let locale = config.load(&shell.user_data_dir, "locale")?
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|locale| locale.get("locale") == settings.get("locale"))
        .unwrap_or_else(|| json!({}));
    let position_path = shell.user_data_dir.join("window-position.json");
    let position: WindowPosition = match std::fs::read_to_string(position_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => WindowPosition::default(),
        Err(err) => return Err(err.to_string()),
    };
    let state = app.state::<WindowState>();
    *state.position.lock().map_err(|err| err.to_string())? = position.clone();
    *state.flags.lock().map_err(|err| err.to_string())? = WindowFlags {
        maximized: position.maximized, fullscreen: position.full_screen, minimized: false,
    };
    shell.window_ready.store(false, Ordering::SeqCst);
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .title("KeeWeb")
        .inner_size(1000.0, 700.0)
        .min_inner_size(700.0, 400.0)
        .background_color(background_color(&settings, true))
        .visible(false);
    // Debug builds mirror the webview console to stderr (`dev_log` command), there is no CDP in wry.
    let builder = if cfg!(debug_assertions) || std::env::var_os("KEEWEB_STARTUP_LOGGING").is_some() {
        builder.initialization_script(include_str!("dev-console.js"))
    } else { builder };
    // KEEWEB_DEV_SMOKE=<file.js>: injected into the webview for scripted smoke tests (debug only).
    let builder = match std::env::var_os("KEEWEB_DEV_SMOKE").filter(|_| cfg!(debug_assertions)) {
        Some(path) => builder.initialization_script(std::fs::read_to_string(path).map_err(|err| err.to_string())?),
        None => builder,
    };
    #[cfg(target_os = "macos")]
    let builder = if matches!(settings.get("titlebarStyle").and_then(Value::as_str), Some("hidden" | "hidden-inset" | "hiddenInset")) {
        builder.title_bar_style(tauri::TitleBarStyle::Overlay)
    } else { builder };
    #[cfg(target_os = "windows")]
    let builder = if matches!(settings.get("titlebarStyle").and_then(Value::as_str), Some("hidden" | "hidden-inset" | "hiddenInset")) {
        builder.decorations(false)
    } else { builder };
    let window = builder.build().map_err(|err| err.to_string())?;
    let dark = window.theme().map_err(|err| err.to_string())? == tauri::Theme::Dark;
    window.set_background_color(Some(background_color(&settings, dark))).map_err(|err| err.to_string())?;
    apply_menu(app, &locale).map_err(|err| err.to_string())?;
    if let (Some(x), Some(y), Some(width), Some(height)) = (position.x, position.y, position.width, position.height) {
        if [x, y, width, height].iter().all(|value| value.is_finite()) && width > 0.0 && height > 0.0 {
            window.set_position(LogicalPosition::new(x, y)).map_err(|err| err.to_string())?;
            let scale = window.scale_factor().map_err(|err| err.to_string())?;
            let outer = window.outer_size().map_err(|err| err.to_string())?;
            let inner = window.inner_size().map_err(|err| err.to_string())?;
            window.set_size(LogicalSize::new(
                (width - f64::from(outer.width.saturating_sub(inner.width)) / scale).max(700.0),
                (height - f64::from(outer.height.saturating_sub(inner.height)) / scale).max(400.0),
            )).map_err(|err| err.to_string())?;
        }
    }
    coerce_to_monitor(&window)?;
    if position.maximized {
        window.maximize().map_err(|err| err.to_string())?;
    }
    if position.full_screen {
        window.set_fullscreen(true).map_err(|err| err.to_string())?;
    }
    if std::env::args().any(|arg| arg.starts_with("--devtools"))
        || std::env::var("KEEWEB_OPEN_DEVTOOLS").as_deref() == Ok("1")
    {
        window.open_devtools();
    }
    Ok(window)
}

fn coerce_to_monitor(window: &WebviewWindow) -> Result<(), String> {
    let monitors = window.available_monitors().map_err(|err| err.to_string())?;
    if monitors.is_empty() {
        return Ok(());
    }
    let position = window.outer_position().map_err(|err| err.to_string())?;
    let outer = window.outer_size().map_err(|err| err.to_string())?;
    let inner_position = window.inner_position().map_err(|err| err.to_string())?;
    let scale = window.scale_factor().map_err(|err| err.to_string())?;
    let titlebar_height = f64::from(inner_position.y - position.y).max(28.0 * scale);
    for monitor in &monitors {
        let area = monitor.work_area();
        let overlap_width = (f64::from(position.x) + f64::from(outer.width)).min(f64::from(area.position.x) + f64::from(area.size.width)) - f64::from(position.x.max(area.position.x));
        let overlap_height = (f64::from(position.y) + titlebar_height).min(f64::from(area.position.y) + f64::from(area.size.height)) - f64::from(position.y.max(area.position.y));
        if overlap_width >= 160.0 * scale && 3.0 * overlap_height >= 2.0 * titlebar_height {
            return Ok(());
        }
    }
    let primary = window.primary_monitor().map_err(|err| err.to_string())?.unwrap_or_else(|| monitors[0].clone());
    let area = primary.work_area();
    let width = outer.width.min((f64::from(area.size.width) * 0.9) as u32);
    let height = outer.height.min((f64::from(area.size.height) * 0.9) as u32);
    let inner = window.inner_size().map_err(|err| err.to_string())?;
    window.set_size(PhysicalSize::new(width.saturating_sub(outer.width.saturating_sub(inner.width)), height.saturating_sub(outer.height.saturating_sub(inner.height))))
        .map_err(|err| err.to_string())?;
    window.set_position(PhysicalPosition::new(area.position.x + ((area.size.width - width) / 2) as i32, area.position.y + ((area.size.height - height) / 2) as i32))
        .map_err(|err| err.to_string())
}

fn update_position(app: &AppHandle) -> Result<(), String> {
    let window = main_window(app)?;
    let maximized = window.is_maximized().map_err(|err| err.to_string())?;
    let fullscreen = window.is_fullscreen().map_err(|err| err.to_string())?;
    let minimized = window.is_minimized().map_err(|err| err.to_string())?;
    let bounds = if !maximized && !fullscreen && !minimized {
        let scale = window.scale_factor().map_err(|err| err.to_string())?;
        let position = window.outer_position().map_err(|err| err.to_string())?.to_logical::<f64>(scale);
        let size = window.outer_size().map_err(|err| err.to_string())?.to_logical::<f64>(scale);
        Some((position, size))
    } else { None };
    let state = app.state::<WindowState>();
    {
        let mut position = state.position.lock().map_err(|err| err.to_string())?;
        if let Some((point, size)) = bounds {
            position.x = Some(point.x);
            position.y = Some(point.y);
            position.width = Some(size.width);
            position.height = Some(size.height);
        }
        if !minimized {
            position.maximized = maximized;
            position.full_screen = fullscreen;
        }
    }
    let mut flags = state.flags.lock().map_err(|err| err.to_string())?;
    if minimized && !flags.minimized {
        emit_app_event(app, "launcher-minimize", Value::Null);
    }
    if !minimized && maximized != flags.maximized {
        emit_app_event(app, if maximized { "launcher-maximize" } else { "launcher-unmaximize" }, Value::Null);
        flags.maximized = maximized;
    }
    if !minimized && fullscreen != flags.fullscreen {
        emit_app_event(app, if fullscreen { "enter-full-screen" } else { "leave-full-screen" }, Value::Null);
        flags.fullscreen = fullscreen;
    }
    flags.minimized = minimized;
    Ok(())
}

pub fn save_position(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window("main").is_some() {
        update_position(app)?;
    }
    let state = app.state::<WindowState>();
    let position = state.position.lock().map_err(|err| err.to_string())?;
    let json = serde_json::to_vec(&*position).map_err(|err| err.to_string())?;
    std::fs::write(app.state::<Shell>().user_data_dir.join("window-position.json"), json).map_err(|err| err.to_string())
}

pub fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != "main" || window.app_handle().try_state::<WindowState>().is_none() {
        return;
    }
    let app = window.app_handle();
    match event {
        WindowEvent::Focused(focused) => {
            emit_app_event(app, if *focused { "main-window-focus" } else { "main-window-blur" }, Value::Null);
            let _ = update_position(app);
        }
        WindowEvent::Moved(_) | WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
            if let Err(err) = update_position(app) {
                emit_app_event(app, "log", json!(err));
            }
            let _ = app.state::<WindowState>().save_signal.send(());
        }
        WindowEvent::CloseRequested { api, .. } => {
            if let Err(err) = save_position(app) {
                emit_app_event(app, "log", json!(err));
            }
            let shell = app.state::<Shell>();
            if shell.hook_before_quit.load(Ordering::SeqCst) && !shell.exit_requested.load(Ordering::SeqCst) {
                api.prevent_close();
                emit_app_event(app, "launcher-before-quit", Value::Null);
            } else if !shell.exit_requested.load(Ordering::SeqCst) && shell.tray.lock().is_ok_and(|tray| tray.is_some()) {
                api.prevent_close();
                if let Err(err) = minimize_app(app.clone(), TrayLabels::default()) {
                    emit_app_event(app, "log", json!(err));
                }
            }
        }
        WindowEvent::Destroyed => {
            let state = app.state::<WindowState>();
            *state.flags.lock().unwrap_or_else(|err| err.into_inner()) = WindowFlags::default();
            let was_teardown = state.destroying.swap(false, Ordering::SeqCst);
            let shell = app.state::<Shell>();
            shell.window_ready.store(false, Ordering::SeqCst);
            shell.hook_before_quit.store(false, Ordering::SeqCst);
            if was_teardown && !shell.hidden_in_tray.load(Ordering::SeqCst) && !shell.exit_requested.load(Ordering::SeqCst) {
                // A restore can arrive after destroy() was dispatched but before this event.
                if let Err(err) = show_main_window(app.clone()) {
                    emit_app_event(app, "log", json!(err));
                }
            }
        }
        _ => {}
    }
}

fn schedule_teardown(app: &AppHandle) {
    let shell = app.state::<Shell>();
    let generation = shell.teardown_generation.fetch_add(1, Ordering::SeqCst) + 1;
    if !shell.hidden_in_tray.load(Ordering::SeqCst)
        || shell.has_open_files.load(Ordering::SeqCst)
        || !shell.window_ready.load(Ordering::SeqCst)
        || shell.exit_requested.load(Ordering::SeqCst)
        || app.get_webview_window("main").is_none()
    {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let handle = app.clone();
        if let Err(err) = app.run_on_main_thread(move || {
            let shell = handle.state::<Shell>();
            if shell.teardown_generation.load(Ordering::SeqCst) != generation
                || !shell.hidden_in_tray.load(Ordering::SeqCst)
                || shell.has_open_files.load(Ordering::SeqCst)
                || !shell.window_ready.load(Ordering::SeqCst)
                || shell.exit_requested.load(Ordering::SeqCst)
            {
                return;
            }
            if let Some(window) = handle.get_webview_window("main") {
                if let Err(err) = save_position(&handle) {
                    emit_app_event(&handle, "log", json!(err));
                    return;
                }
                let state = handle.state::<WindowState>();
                state.destroying.store(true, Ordering::SeqCst);
                shell.window_ready.store(false, Ordering::SeqCst);
                if let Err(err) = window.destroy() {
                    state.destroying.store(false, Ordering::SeqCst);
                    shell.window_ready.store(true, Ordering::SeqCst);
                    emit_app_event(&handle, "log", json!(err.to_string()));
                }
            }
        }) {
            emit_app_event(&app, "log", json!(err.to_string()));
        }
    });
}

#[tauri::command]
pub fn set_has_open_files(app: AppHandle, shell: State<'_, Shell>, has_open_files: bool) -> Result<(), String> {
    shell.has_open_files.store(has_open_files, Ordering::SeqCst);
    if has_open_files {
        shell.teardown_generation.fetch_add(1, Ordering::SeqCst);
    } else {
        schedule_teardown(&app);
    }
    Ok(())
}

#[tauri::command]
pub fn window_ready(app: AppHandle) -> Result<(), String> {
    main_window(&app)?;
    let shell = app.state::<Shell>();
    let events = {
        let mut pending = shell.pending_events.lock().map_err(|err| err.to_string())?;
        shell.window_ready.store(true, Ordering::SeqCst);
        std::mem::take(&mut *pending)
    };
    for (name, data) in events {
        emit_app_event(&app, &name, data);
    }
    schedule_teardown(&app);
    Ok(())
}

#[tauri::command]
pub fn minimize_app(app: AppHandle, labels: TrayLabels) -> Result<(), String> {
    let window = main_window(&app)?;
    save_position(&app)?;
    let shell = app.state::<Shell>();
    {
        let mut tray = shell.tray.lock().map_err(|err| err.to_string())?;
        if tray.is_none() {
            let restore = MenuItem::with_id(&app, "tray-restore", labels.restore, true, None::<&str>).map_err(|err| err.to_string())?;
            let quit = MenuItem::with_id(&app, "tray-quit", labels.quit, true, None::<&str>).map_err(|err| err.to_string())?;
            let menu = Menu::with_items(&app, &[&restore, &quit]).map_err(|err| err.to_string())?;
            #[cfg(target_os = "macos")]
            let icon = Image::from_bytes(include_bytes!("../icons/macOS-MenubarTemplate.png"));
            #[cfg(not(target_os = "macos"))]
            let icon = Image::from_bytes(include_bytes!("../icons/tray.png"));
            let builder = TrayIconBuilder::with_id("keeweb-tray")
                .icon(icon.map_err(|err| err.to_string())?)
                .icon_as_template(cfg!(target_os = "macos"))
                .tooltip("KeeWeb")
                .menu(&menu)
                .show_menu_on_left_click(cfg!(target_os = "macos"))
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "tray-restore" => {
                        if let Err(err) = show_main_window(app.clone()) {
                            emit_app_event(app, "log", json!(err));
                        }
                    }
                    "tray-quit" => request_quit(app),
                    _ => {}
                });
            #[cfg(not(target_os = "macos"))]
            let builder = builder.on_tray_icon_event(|tray, event| {
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
                if matches!(event, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) {
                    if let Err(err) = show_main_window(tray.app_handle().clone()) {
                        emit_app_event(tray.app_handle(), "log", json!(err));
                    }
                }
            });
            *tray = Some(builder.build(&app).map_err(|err| err.to_string())?);
        }
    }
    #[cfg(target_os = "windows")]
    window.minimize().map_err(|err| err.to_string())?;
    window.hide().map_err(|err| err.to_string())?;
    #[cfg(not(target_os = "macos"))]
    window.set_skip_taskbar(true).map_err(|err| err.to_string())?;
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory).map_err(|err| err.to_string())?;
    shell.hidden_in_tray.store(true, Ordering::SeqCst);
    schedule_teardown(&app);
    Ok(())
}

#[tauri::command]
pub fn minimize_then_hide_if_in_tray(app: AppHandle) -> Result<(), String> {
    let window = main_window(&app)?;
    save_position(&app)?;
    window.minimize().map_err(|err| err.to_string())?;
    if app.state::<Shell>().tray.lock().map_err(|err| err.to_string())?.is_some() {
        window.hide().map_err(|err| err.to_string())?;
        app.state::<Shell>().hidden_in_tray.store(true, Ordering::SeqCst);
        schedule_teardown(&app);
    }
    update_position(&app)
}

#[tauri::command]
pub fn show_main_window(app: AppHandle) -> Result<(), String> {
    let shell = app.state::<Shell>();
    shell.teardown_generation.fetch_add(1, Ordering::SeqCst);
    shell.hidden_in_tray.store(false, Ordering::SeqCst);
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let state = handle.state::<WindowState>();
        if state.destroying.load(Ordering::SeqCst) || handle.state::<Shell>().exit_requested.load(Ordering::SeqCst) {
            return;
        }
        if let Some(window) = handle.get_webview_window("main") {
            if let Err(err) = show_window(&handle, &window) {
                emit_app_event(&handle, "log", json!(err));
            }
        } else if !state.creating.swap(true, Ordering::SeqCst) {
            // WebView2 creation deadlocks in synchronous commands and event callbacks.
            let app = handle.clone();
            std::thread::spawn(move || {
                let created = create_main_window(&app);
                app.state::<WindowState>().creating.store(false, Ordering::SeqCst);
                // Tray removal (NSStatusItem) and activation must happen on the main thread.
                let handle = app.clone();
                let result = created.and_then(|window| {
                    handle.run_on_main_thread(move || {
                        if let Err(err) = show_window(&app, &window) {
                            emit_app_event(&app, "log", json!(err));
                        }
                    }).map_err(|err| err.to_string())
                });
                if let Err(err) = result {
                    emit_app_event(&handle, "log", json!(err));
                }
            });
        }
    }).map_err(|err| err.to_string())
}

fn show_window(app: &AppHandle, window: &WebviewWindow) -> Result<(), String> {
    let maximized = app.state::<WindowState>().position.lock().map_err(|err| err.to_string())?.maximized;
    #[cfg(target_os = "macos")]
    {
        app.set_activation_policy(tauri::ActivationPolicy::Regular).map_err(|err| err.to_string())?;
        app.show().map_err(|err| err.to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    window.set_skip_taskbar(false).map_err(|err| err.to_string())?;
    window.show().map_err(|err| err.to_string())?;
    window.unminimize().map_err(|err| err.to_string())?;
    coerce_to_monitor(window)?;
    if maximized {
        window.maximize().map_err(|err| err.to_string())?;
    }
    window.set_focus().map_err(|err| err.to_string())?;
    if cfg!(debug_assertions) && std::env::var_os("KEEWEB_DEV_SMOKE").is_some() {
        // Smoke runs need a visible page: WebKit freezes timers/rAF for occluded windows.
        window.set_visible_on_all_workspaces(true).map_err(|err| err.to_string())?;
        window.set_always_on_top(true).map_err(|err| err.to_string())?;
    }
    let shell = app.state::<Shell>();
    shell.hidden_in_tray.store(false, Ordering::SeqCst);
    let tray = shell.tray.lock().map_err(|err| err.to_string())?.take();
    if let Some(tray) = tray {
        app.remove_tray_by_id(tray.id());
    }
    Ok(())
}

#[tauri::command]
pub fn hide_app(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        main_window(&app)?;
        app.hide().map_err(|err| err.to_string())?;
        let shell = app.state::<Shell>();
        if shell.tray.lock().map_err(|err| err.to_string())?.is_some() {
            shell.hidden_in_tray.store(true, Ordering::SeqCst);
            schedule_teardown(&app);
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    { minimize_then_hide_if_in_tray(app) }
}

#[tauri::command]
pub fn is_app_focused(app: AppHandle) -> Result<bool, String> {
    main_window(&app)?.is_focused().map_err(|err| err.to_string())
}

#[tauri::command]
pub fn set_hook_before_quit(shell: State<'_, Shell>, hooked: bool) -> Result<(), String> {
    shell.hook_before_quit.store(hooked, Ordering::SeqCst);
    Ok(())
}

pub fn request_quit(app: &AppHandle) {
    if app.get_webview_window("main").is_none() {
        if let Err(err) = quit_app(app.clone()) {
            emit_app_event(app, "log", json!(err));
        }
    } else {
        emit_app_event(app, "launcher-exit-request", Value::Null);
    }
}

#[tauri::command]
pub fn quit_app(app: AppHandle) -> Result<(), String> {
    if app.get_webview_window("main").is_some() {
        save_position(&app)?;
    }
    app.state::<Shell>().exit_requested.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

#[cfg(target_os = "macos")]
fn apply_menu(app: &AppHandle, values: &Value) -> tauri::Result<()> {
    use tauri::menu::{AboutMetadata, PredefinedMenuItem as Item, Submenu};
    let label = |key: &str, fallback: &str| values.get(key).and_then(Value::as_str).unwrap_or(fallback).replace("{}", "KeeWeb");
    let app_menu = Submenu::with_items(app, "KeeWeb", true, &[
        &Item::about(app, Some(&label("sysMenuAboutKeeWeb", "About KeeWeb")), Some(AboutMetadata {
            name: Some("KeeWeb".into()), version: Some(env!("CARGO_PKG_VERSION").into()), ..Default::default()
        }))?,
        &Item::separator(app)?,
        &Item::services(app, Some(&label("sysMenuServices", "Services")))?,
        &Item::separator(app)?,
        &Item::hide(app, Some(&label("sysMenuHide", "Hide KeeWeb")))?,
        &Item::hide_others(app, Some(&label("sysMenuHideOthers", "Hide Others")))?,
        &Item::show_all(app, Some(&label("sysMenuUnhide", "Show All")))?,
        &Item::separator(app)?,
        &MenuItem::with_id(app, "app-quit", label("sysMenuQuit", "Quit KeeWeb"), true, Some("Cmd+Q"))?,
    ])?;
    let edit_menu = Submenu::with_items(app, label("sysMenuEdit", "Edit"), true, &[
        &Item::undo(app, Some(&label("sysMenuUndo", "Undo")))?,
        &Item::redo(app, Some(&label("sysMenuRedo", "Redo")))?,
        &Item::separator(app)?,
        &Item::cut(app, Some(&label("sysMenuCut", "Cut")))?,
        &Item::copy(app, Some(&label("sysMenuCopy", "Copy")))?,
        &Item::paste(app, Some(&label("sysMenuPaste", "Paste")))?,
        &Item::select_all(app, Some(&label("sysMenuSelectAll", "Select All")))?,
    ])?;
    let window_menu = Submenu::with_items(app, label("sysMenuWindow", "Window"), true, &[
        &Item::minimize(app, Some(&label("sysMenuMinimize", "Minimize")))?,
        &Item::close_window(app, Some(&label("sysMenuClose", "Close")))?,
    ])?;
    app.set_menu(Menu::with_items(app, &[&app_menu, &edit_menu, &window_menu])?)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apply_menu(app: &AppHandle, _values: &Value) -> tauri::Result<()> {
    app.remove_menu()?;
    Ok(())
}

#[tauri::command]
pub fn set_menu_labels(app: AppHandle, values: Value) -> Result<(), String> {
    apply_menu(&app, &values).map_err(|err| err.to_string())?;
    let data = serde_json::to_string(&values).map_err(|err| err.to_string())?;
    app.state::<ConfigStore>().save(&app.state::<Shell>().user_data_dir, "locale", &data)
}

#[tauri::command]
pub fn open_devtools(app: AppHandle) -> Result<(), String> {
    main_window(&app)?.open_devtools();
    Ok(())
}

#[tauri::command]
pub fn resolve_proxy(url: String) -> Result<Option<Value>, String> {
    // Wry does not expose system proxy resolution; Phase 1 uses direct requests.
    let _ = url;
    Ok(None)
}

#[tauri::command]
pub fn get_startup_info(shell: State<'_, Shell>) -> Result<StartupInfo, String> {
    let mut startup = shell.startup.clone();
    if shell.startup_info_read.swap(true, Ordering::SeqCst) {
        startup.open_file = None;
        startup.open_keyfile = None;
        startup.start_minimized = false;
    }
    Ok(startup)
}
