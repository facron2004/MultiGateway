use std::sync::Arc;
use tauri::{
    Manager, RunEvent, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

use crate::{native_commands as native, state::AppState, tray};

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

static MAIN_APP_HANDLE: parking_lot::Mutex<Option<tauri::AppHandle>> =
    parking_lot::Mutex::new(None);

pub fn set_main_app_handle(handle: tauri::AppHandle) {
    *MAIN_APP_HANDLE.lock() = Some(handle);
}

pub fn reveal_main_window() -> bool {
    let handle = MAIN_APP_HANDLE.lock().clone();
    if let Some(app) = handle {
        crate::tray::show_main(&app, None, None);
        true
    } else {
        false
    }
}

pub fn run_desktop_window(
    context: tauri::Context<tauri::Wry>,
    state: AppState,
    silent: bool,
    quit: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    tray::mark_started();
    let webview_data_dir = state.config.data_dir.join("webview2");
    std::fs::create_dir_all(&webview_data_dir)?;

    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            desktop_action,
            tray::tray_summary,
            tray::tray_action,
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
            native::provider_settings_save,
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
            set_main_app_handle(app.handle().clone());
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title(concat!("MultiGateway · ", env!("CARGO_PKG_VERSION")))
                .inner_size(980.0, 640.0)
                .min_inner_size(700.0, 480.0)
                .decorations(false)
                .visible(!silent)
                .data_directory(webview_data_dir.clone())
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
            {
                app.manage(TrayPanelState::default());
                create_tray_panel(app, &webview_data_dir)?;
                create_tray(app)?;
                start_tooltip_updates(app);
            }
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

    // A macOS app that owns a tray icon keeps running after its last window is
    // closed. This bundle has no Dock icon wired to a window, so that leaves the
    // gateway running invisibly and the next launch colliding on the port.
    // Closing the window is therefore a real quit unless close_to_tray hides it.
    #[cfg(target_os = "macos")]
    if let Some(window) = app.get_webview_window("main") {
        let handle = app.handle().clone();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed) {
                handle.exit(0);
            }
        });
    }

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
#[derive(Default)]
struct TrayPanelState {
    rect: parking_lot::Mutex<Option<tauri::Rect>>,
    hidden_at: parking_lot::Mutex<Option<std::time::Instant>>,
    shown_at: parking_lot::Mutex<Option<std::time::Instant>>,
}

#[cfg(any(windows, target_os = "macos"))]
impl TrayPanelState {
    fn remember_rect(&self, rect: tauri::Rect) {
        *self.rect.lock() = Some(rect);
    }

    fn last_rect(&self) -> Option<tauri::Rect> {
        *self.rect.lock()
    }

    fn mark_hidden(&self) {
        *self.hidden_at.lock() = Some(std::time::Instant::now());
    }

    fn mark_shown(&self) {
        *self.shown_at.lock() = Some(std::time::Instant::now());
    }

    /// Windows can deny foreground activation to a panel opened from the tray,
    /// which would otherwise blur-hide it the instant it appears.
    fn just_shown(&self) -> bool {
        let shown_at = *self.shown_at.lock();
        shown_at.is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(400))
    }

    /// Clicking the tray icon blurs the panel before the click handler runs.
    /// Without this window, toggling the panel would immediately reopen it.
    fn just_hidden(&self) -> bool {
        let hidden_at = *self.hidden_at.lock();
        hidden_at.is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(350))
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn create_tray_panel(
    app: &tauri::App,
    webview_data_dir: &std::path::Path,
) -> tauri::Result<WebviewWindow> {
    let builder = WebviewWindowBuilder::new(app, "tray", WebviewUrl::App("tray.html".into()))
        .title("MultiGateway")
        .inner_size(380.0, 680.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .visible(false)
        .focused(false)
        .data_directory(webview_data_dir.to_path_buf())
        .on_navigation(|url| is_app_url(url))
        .on_new_window(|url, _| {
            let _ = open_external(url.as_str());
            tauri::webview::NewWindowResponse::Deny
        });
    #[cfg(windows)]
    let builder = builder.no_redirection_bitmap(true);
    let window = builder.build()?;
    let handle = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Focused(false) => {
            let panel = handle.state::<TrayPanelState>();
            if panel.just_shown() {
                return;
            }
            let _ = handle.hide();
            panel.mark_hidden();
        }
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = handle.hide();
        }
        _ => {}
    });
    Ok(window)
}

#[cfg(any(windows, target_os = "macos"))]
fn toggle_tray_panel(app: &tauri::AppHandle, rect: Option<tauri::Rect>) {
    let Some(window) = app.get_webview_window("tray") else {
        return;
    };
    let panel = app.state::<TrayPanelState>();
    if let Some(rect) = rect {
        panel.remember_rect(rect);
    }
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        panel.mark_hidden();
        return;
    }
    if panel.just_hidden() {
        return;
    }
    let rect = rect.or_else(|| panel.last_rect());
    if let Some((position, size)) = tray_panel_geometry(app, &window, rect) {
        if window.outer_size().ok() != Some(size) {
            let _ = window.set_size(size);
        }
        let _ = window.set_position(position);
    }
    let _ = window.show();
    panel.mark_shown();
    let _ = window.set_focus();
}

#[cfg(any(windows, target_os = "macos"))]
fn tray_panel_geometry(
    app: &tauri::AppHandle,
    window: &WebviewWindow,
    rect: Option<tauri::Rect>,
) -> Option<(tauri::PhysicalPosition<i32>, tauri::PhysicalSize<u32>)> {
    use tauri::{PhysicalPosition, PhysicalSize};

    // The cursor reliably identifies the monitor even when a menu item opens
    // the panel after the tray rect was recorded.
    let cursor = app.cursor_position().ok()?;
    let monitor = app
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .or_else(|| window.current_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let point = rect
        .map(|rect| rect.position.to_physical::<f64>(scale))
        .unwrap_or(cursor);
    let icon = rect
        .map(|rect| rect.size.to_physical::<f64>(scale))
        .unwrap_or(PhysicalSize::new(16.0, 16.0));

    let gap = (12.0 * scale).round() as i32;
    let work = monitor.work_area();
    let width = (380.0 * scale).round() as u32;
    // A 768p screen at 125% scaling only leaves ~614 logical pixels of work
    // area; cap the panel so it never hangs off the bottom of the screen.
    let available_height = work.size.height as i32 - gap * 2;
    let height = ((680.0 * scale).round() as i32)
        .min(available_height)
        .max((420.0 * scale).round() as i32) as u32;
    let size = PhysicalSize::new(width, height);

    let left = work.position.x + gap;
    let top = work.position.y + gap;
    let right = work.position.x + work.size.width as i32 - width as i32 - gap;
    let bottom = work.position.y + work.size.height as i32 - height as i32 - gap;

    let icon_center_y = point.y + icon.height / 2.0;
    let work_center_y = work.position.y as f64 + work.size.height as f64 / 2.0;
    let x = (point.x + icon.width / 2.0 - width as f64 / 2.0).round() as i32;
    let y = if icon_center_y >= work_center_y {
        (point.y - height as f64 - gap as f64).round() as i32
    } else {
        (point.y + icon.height + gap as f64).round() as i32
    };
    Some((
        PhysicalPosition::new(
            x.clamp(left, right.max(left)),
            y.clamp(top, bottom.max(top)),
        ),
        size,
    ))
}

#[cfg(any(windows, target_os = "macos"))]
fn start_tooltip_updates(app: &tauri::App) {
    let handle = app.handle().clone();
    let state = app.state::<AppState>().inner().clone();
    tauri::async_runtime::spawn(async move {
        loop {
            if let Some(tray) = handle.tray_by_id("main") {
                let summary = tray::summary(&state);
                let _ = tray.set_tooltip(Some(tray::tooltip(&summary)));
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

#[cfg(any(windows, target_os = "macos"))]
fn create_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let icon =
        tauri::image::Image::new_owned(include_bytes!("../assets/tray-64.rgba").to_vec(), 64, 64);
    TrayIconBuilder::with_id("main")
        .tooltip("MultiGateway · 网关运行中")
        .icon(icon)
        .on_tray_icon_event(|tray, event| {
            let app = tray.app_handle();
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => tray::show_main(app, None, None),
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    rect,
                    ..
                } => toggle_tray_panel(app, Some(rect)),
                TrayIconEvent::Enter { rect, .. } | TrayIconEvent::Move { rect, .. } => {
                    app.state::<TrayPanelState>().remember_rect(rect);
                }
                _ => {}
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
