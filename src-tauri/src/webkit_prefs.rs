//! macOS WKWebView tuning through WebKit's (private) feature flags.
//!
//! Every `overflow: scroll` element becomes its own full-size compositing layer when
//! async overflow scrolling is on; KeeWeb has several of them, each costing a
//! window-sized IOSurface (~11 MB at 1000x700@2x). Main-thread scrolling is fine for
//! this app, so we turn the feature off.

use tauri::WebviewWindow;

#[cfg(target_os = "macos")]
pub fn apply(window: &WebviewWindow) {
    use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_foundation::{NSArray, NSString};

    let disabled: Vec<String> = std::env::var("KEEWEB_WEBKIT_DISABLE")
        .map(|list| list.split(',').map(str::to_owned).collect())
        .unwrap_or_else(|_| vec!["AsyncOverflowScrollingEnabled".to_owned()]);
    if disabled.is_empty() {
        return;
    }
    let result = window.with_webview(move |webview| unsafe {
        let wk: &AnyObject = &*(webview.inner().cast::<AnyObject>());
        let configuration: Retained<AnyObject> = msg_send![wk, configuration];
        let preferences: Retained<AnyObject> = msg_send![&*configuration, preferences];
        for (selector_list, setter_is_internal) in [("_internalDebugFeatures", true), ("_experimentalFeatures", false)] {
            let features: Retained<NSArray<AnyObject>> = match selector_list {
                "_internalDebugFeatures" => msg_send![class!(WKPreferences), _internalDebugFeatures],
                _ => msg_send![class!(WKPreferences), _experimentalFeatures],
            };
            for feature in features.iter() {
                let key: Retained<NSString> = msg_send![&*feature, key];
                if disabled.iter().any(|name| key.to_string() == *name) {
                    if setter_is_internal {
                        let _: () = msg_send![&*preferences, _setEnabled: false, forInternalDebugFeature: &*feature];
                    } else {
                        let _: () = msg_send![&*preferences, _setEnabled: false, forExperimentalFeature: &*feature];
                    }
                    eprintln!("webkit: disabled {key}");
                }
            }
        }
    });
    if let Err(err) = result {
        eprintln!("webkit: cannot apply preferences: {err}");
    }
}

/// Offline enforcement inside WebKit: a content rule list that blocks every URL
/// except the app's own origins. Applies to fetch/XHR/WebSocket/img/font/frames.
#[cfg(target_os = "macos")]
pub fn block_network(window: &WebviewWindow) {
    use block2::RcBlock;
    use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_foundation::NSString;

    let dev = if cfg!(debug_assertions) { r#",{"trigger":{"url-filter":"^http://localhost:8085/"},"action":{"type":"ignore-previous-rules"}}"# } else { "" };
    let rules = format!(
        r#"[{{"trigger":{{"url-filter":".*"}},"action":{{"type":"block"}}}},{{"trigger":{{"url-filter":"^tauri://localhost/"}},"action":{{"type":"ignore-previous-rules"}}}},{{"trigger":{{"url-filter":"^http://ipc\\.localhost/"}},"action":{{"type":"ignore-previous-rules"}}}}{dev}]"#
    );
    let result = window.with_webview(move |webview| unsafe {
        let wk: &AnyObject = &*(webview.inner().cast::<AnyObject>());
        let configuration: Retained<AnyObject> = msg_send![wk, configuration];
        let controller: Retained<AnyObject> = msg_send![&*configuration, userContentController];
        let store: Retained<AnyObject> = msg_send![class!(WKContentRuleListStore), defaultStore];
        let identifier = NSString::from_str("lemonkee-offline");
        let source = NSString::from_str(&rules);
        let handler = RcBlock::new(move |list: *mut AnyObject, error: *mut AnyObject| {
            if list.is_null() {
                let description: Retained<NSString> = msg_send![error, localizedDescription];
                eprintln!("webkit: cannot compile offline rule list: {description}");
                return;
            }
            let _: () = msg_send![&*controller, addContentRuleList: list];
            eprintln!("webkit: offline rule list active");
        });
        let _: () = msg_send![&*store, compileContentRuleListForIdentifier: &*identifier, encodedContentRuleList: &*source, completionHandler: &*handler];
    });
    if let Err(err) = result {
        eprintln!("webkit: cannot install offline rule list: {err}");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn block_network(_window: &WebviewWindow) {}

#[cfg(not(target_os = "macos"))]
pub fn apply(_window: &WebviewWindow) {}
