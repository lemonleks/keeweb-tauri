use tauri::AppHandle;

/// Install process-lifetime power observers once, from the app's main-thread setup hook.
pub fn start(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    macos::start(app);
    #[cfg(target_os = "windows")]
    windows::start(app);
    // KeeWeb does not advertise sleep detection on Linux.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = app;
}

#[cfg(target_os = "macos")]
mod macos {
    use super::AppHandle;
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification};
    use objc2_foundation::{
        NSDistributedNotificationCenter, NSNotification, NSNotificationCenter, NSString,
    };
    use std::ptr::NonNull;

    fn observe(app: &AppHandle, center: &NSNotificationCenter, name: &NSString, event: &'static str) {
        let app = app.clone();
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            crate::emit_app_event(&app, event, serde_json::Value::Null);
        });
        // The captured AppHandle is Send + Sync; no object filter or operation queue is used.
        let observer = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        // Intentionally leak the observer token: the center retains its block and these
        // subscriptions must stay alive until process exit, including while the window hides.
        let _ = Retained::into_raw(observer);
    }

    pub(super) fn start(app: &AppHandle) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        unsafe {
            observe(app, &center, NSWorkspaceWillSleepNotification, "power-monitor-suspend");
            observe(app, &center, NSWorkspaceDidWakeNotification, "power-monitor-resume");
        }
        observe(
            app,
            &NSDistributedNotificationCenter::defaultCenter(),
            &NSString::from_str("com.apple.screenIsLocked"),
            "os-lock",
        );
        // Display sleep alone is not OS suspend and must not lock databases.
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::AppHandle;
    use std::cell::OnceCell;
    use ::windows::{
        core::{w, Error, Result},
        Win32::{
            Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            System::{
                LibraryLoader::GetModuleHandleW,
                Power::{RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification},
                RemoteDesktop::{
                    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
                    NOTIFY_FOR_THIS_SESSION,
                },
            },
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
                GetWindowLongPtrW, RegisterClassW, SetWindowLongPtrW, TranslateMessage,
                UnregisterClassW, DEVICE_NOTIFY_WINDOW_HANDLE, GWLP_USERDATA, HWND_MESSAGE, MSG,
                PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ENDSESSION,
                WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSW, WTS_SESSION_LOCK,
            },
        },
    };

    thread_local! {
        static APP: OnceCell<AppHandle> = const { OnceCell::new() };
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let event = match message {
            WM_POWERBROADCAST if GetWindowLongPtrW(hwnd, GWLP_USERDATA) == 1 => {
                match wparam.0 as u32 {
                    PBT_APMSUSPEND => Some("power-monitor-suspend"),
                    PBT_APMRESUMEAUTOMATIC => Some("power-monitor-resume"),
                    _ => None,
                }
            }
            WM_WTSSESSION_CHANGE if wparam.0 as u32 == WTS_SESSION_LOCK => Some("os-lock"),
            WM_ENDSESSION if wparam.0 != 0 => Some("os-lock"),
            _ => None,
        };
        if let Some(event) = event {
            APP.with(|app| {
                if let Some(app) = app.get() {
                    crate::emit_app_event(app, event, serde_json::Value::Null);
                }
            });
        }
        if message == WM_POWERBROADCAST {
            LRESULT(1)
        } else {
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    unsafe fn run() -> Result<()> {
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class_name = w!("KeeWebPowerMonitor");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(Error::from_win32());
        }
        let result = (|| {
            let power_window = CreateWindowExW(
                WINDOW_EX_STYLE::default(), class_name, w!(""), WINDOW_STYLE::default(),
                0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(instance), None,
            )?;
            SetWindowLongPtrW(power_window, GWLP_USERDATA, 1);
            let result = (|| {
                // Message-only windows do not receive WM_ENDSESSION broadcasts. A second,
                // invisible top-level window preserves Electron's session-end -> os-lock.
                let session_window = CreateWindowExW(
                    WINDOW_EX_STYLE::default(), class_name, w!(""), WINDOW_STYLE::default(),
                    0, 0, 0, 0, None, None, Some(instance), None,
                )?;
                let result = (|| {
                    WTSRegisterSessionNotification(power_window, NOTIFY_FOR_THIS_SESSION)?;
                    // Explicit registration delivers suspend/resume to the message-only window.
                    let result = (|| {
                        let power = RegisterSuspendResumeNotification(
                            HANDLE(power_window.0), DEVICE_NOTIFY_WINDOW_HANDLE,
                        )?;
                        let mut message = MSG::default();
                        let result = loop {
                            let status = GetMessageW(&mut message, None, 0, 0).0;
                            if status == -1 {
                                break Err(Error::from_win32());
                            }
                            if status == 0 {
                                break Ok(());
                            }
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        };
                        let _ = UnregisterSuspendResumeNotification(power);
                        result
                    })();
                    let _ = WTSUnRegisterSessionNotification(power_window);
                    result
                })();
                let _ = DestroyWindow(session_window);
                result
            })();
            let _ = DestroyWindow(power_window);
            result
        })();
        let _ = UnregisterClassW(class_name, Some(instance));
        result
    }

    pub(super) fn start(app: &AppHandle) {
        let app = app.clone();
        if let Err(error) = std::thread::Builder::new().name("power-monitor".into()).spawn(move || {
            APP.with(|slot| { let _ = slot.set(app); });
            if let Err(error) = unsafe { run() } {
                eprintln!("Cannot monitor Windows power/session events: {error}");
            }
        }) {
            eprintln!("Cannot start Windows power monitor: {error}");
        }
    }
}
