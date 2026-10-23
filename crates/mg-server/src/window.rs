use std::sync::Arc;
use tauri::{
    Manager, RunEvent, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

use crate::{native_commands as native, state::AppState};

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum DesktopAction {
    Minimize,
    Maximize,
    Close,
    Drag,
    OpenFolder,
    OpenExternal,
}

#[tauri::command]
async fn desktop_action(
    window: WebviewWindow,
    state: State<'_, AppState>,
    action: DesktopAction,
    url: Option<String>,
) -> Result<(), String> {
    match action {
        DesktopAction::Minimize => window.minimize().map_err(|error| error.to_string()),
        DesktopAction::Maximize => {
            let maximized = window.is_maximized().map_err(|error| error.to_string())?;
            if maximized {
                window.unmaximize()
            } else {
                window.maximize()
            }
            .map_err(|error| error.to_string())
        }
        DesktopAction::Close => window.close().map_err(|error| error.to_string()),
        DesktopAction::Drag => window.start_dragging().map_err(|error| error.to_string()),
        DesktopAction::OpenFolder => {
            open::that_detached(&state.config.data_dir).map_err(|error| error.to_string())
        }
        DesktopAction::OpenExternal => open_external(url.as_deref().ok_or("Missing external URL")?),
    }
}

fn external_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|error| error.to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Only HTTP and HTTPS URLs can be opened".into());
    }
    Ok(url)
}

fn open_external(value: &str) -> Result<(), String> {
    let url = external_url(value)?;
    open::that_detached(url.as_str()).map_err(|error| error.to_string())
}

pub fn run_desktop_window(
    context: tauri::Context<tauri::Wry>,
    state: AppState,
    silent: bool,
    quit: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    let webview_data_dir = state.config.data_dir.join("webview2");
    std::fs::create_dir_all(&webview_data_dir)?;

    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            desktop_action,
            native::gateway_health,
            native::gateway_status,
            native::usage_board,
            native::provider_snapshot,
            native::provider_models,
            native::provider_login,
            native::provider_poll,
            native::provider_enable_account,
            native::provider_delete_account,
            native::provider_credits,
            native::provider_credentials_action,
            native::provider_checkin_action,
            native::provider_batch,
            native::provider_region,
            native::provider_import,
            native::gateway_configure,
            native::settings_view,
            native::desktop_settings_save,
            native::appearance_settings_save,
            native::settings_reveal,
            native::api_key_generate,
            native::api_key_delete,
            native::api_key_list,
        ])
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title(concat!("MultiGateway · ", env!("CARGO_PKG_VERSION")))
                .inner_size(980.0, 640.0)
                .min_inner_size(700.0, 480.0)
                .decorations(false)
                .visible(!silent)
                .data_directory(webview_data_dir)
                .initialization_script(include_str!("../ui/desktop.js"))
                .on_navigation(|url| {
                    if is_app_url(url) {
                        true
                    } else {
                        let _ = open_external(url.as_str());
                        false
                    }
                })
                .on_new_window(|url, _| {
                    let _ = open_external(url.as_str());
                    tauri::webview::NewWindowResponse::Deny
                })
                .build()?;
            #[cfg(any(windows, target_os = "macos"))]
            create_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                if cfg!(any(windows, target_os = "macos"))
                    && crate::desktop::load(&state.config.data_dir)
                        .unwrap_or_default()
                        .close_to_tray
                {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(context)?;

    // Return to main so the existing HTTP runtime can drain in-flight requests.
    app.run_return(move |_, event| {
        if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
            quit.notify_one();
        }
    });
    Ok(())
}

fn is_app_url(url: &Url) -> bool {
    url.port().is_none()
        && matches!(
            (url.scheme(), url.host_str()),
            ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
        )
}

#[cfg(any(windows, target_os = "macos"))]
fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn create_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::{
        menu::{Menu, MenuItem},
        tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    };
    let show = MenuItem::with_id(app, "show", "打开 MultiGateway", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出网关", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let icon =
        tauri::image::Image::new_owned(include_bytes!("../assets/tray-64.rgba").to_vec(), 64, 64);
    TrayIconBuilder::with_id("main")
        .tooltip("MultiGateway · 网关运行中")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{external_url, is_app_url};

    #[test]
    fn desktop_navigation_stays_on_the_tauri_asset_origin() {
        for value in [
            "http://tauri.localhost/index.html",
            "tauri://localhost/index.html",
        ] {
            assert!(is_app_url(&value.parse().unwrap()));
        }
        for value in [
            "http://127.0.0.1:8791/",
            "https://example.com",
            "http://tauri.localhost:8791/",
            "file:///index.html",
        ] {
            assert!(!is_app_url(&value.parse().unwrap()));
        }
    }

    #[test]
    fn external_links_allow_oauth_http_urls_and_reject_executable_schemes() {
        for value in [
            "https://github.com/login/device",
            "http://127.0.0.1:8791/callback?code=example",
        ] {
            assert!(external_url(value).is_ok());
        }
        for value in [
            "file:///C:/Windows/System32/cmd.exe",
            "javascript:alert(1)",
            "cmd:calc",
            "invalid",
        ] {
            assert!(external_url(value).is_err());
        }
    }
}
