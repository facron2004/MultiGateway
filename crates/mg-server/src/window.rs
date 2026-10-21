use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

#[derive(Clone, Debug)]
enum DesktopEvent {
    Show,
    Close,
    Quit,
}

pub fn run_desktop_window(
    port: u16,
    data_dir: PathBuf,
    silent: bool,
    quit: Arc<tokio::sync::Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoopBuilder::<DesktopEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = Arc::new(
        WindowBuilder::new()
            .with_title(concat!("MultiGateway · ", env!("CARGO_PKG_VERSION")))
            .with_inner_size(LogicalSize::new(980.0, 640.0))
            .with_min_inner_size(LogicalSize::new(700.0, 480.0))
            .with_decorations(false)
            .with_visible(!silent)
            .build(&event_loop)?,
    );

    #[cfg(windows)]
    let mut tray = Some({
        use tray_icon::{
            menu::{Menu, MenuEvent, MenuItem},
            MouseButton, TrayIconBuilder, TrayIconEvent,
        };
        let menu = Menu::new();
        let show = MenuItem::new("打开 MultiGateway", true, None);
        let quit = MenuItem::new("退出网关", true, None);
        menu.append_items(&[&show, &quit])?;
        let show_id = show.id().clone();
        let quit_id = quit.id().clone();
        let menu_proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == show_id {
                DesktopEvent::Show
            } else if event.id == quit_id {
                DesktopEvent::Quit
            } else {
                return;
            };
            let _ = menu_proxy.send_event(action);
        }));
        let tray_proxy = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                let _ = tray_proxy.send_event(DesktopEvent::Show);
            }
        }));
        TrayIconBuilder::new()
            .with_tooltip("MultiGateway · 网关运行中")
            .with_icon(tray_icon::Icon::from_resource(1, Some((32, 32)))?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?
    });

    let is_maximized = Arc::new(AtomicBool::new(false));
    let win_ipc = window.clone();
    let dir_ipc = data_dir.clone();
    let close_proxy = proxy.clone();
    let init_script = r#"
        window.pywebview = { api: {
            window_minimize: function() { window.ipc.postMessage('minimize'); },
            window_maximize: function() { window.ipc.postMessage('maximize'); },
            window_close: function() { window.ipc.postMessage('close'); },
            open_folder: function() { window.ipc.postMessage('open_folder'); }
        }};
        window.addEventListener('DOMContentLoaded', function() {
            window.dispatchEvent(new Event('pywebviewready'));
        });
        window.dispatchEvent(new Event('pywebviewready'));
    "#;
    let _webview = WebViewBuilder::new()
        .with_url(format!("http://127.0.0.1:{port}/"))
        .with_initialization_script(init_script)
        .with_ipc_handler(move |req| match req.body().as_str() {
            "minimize" => win_ipc.set_minimized(true),
            "maximize" => {
                let maximized = is_maximized.load(Ordering::Relaxed);
                win_ipc.set_maximized(!maximized);
                is_maximized.store(!maximized, Ordering::Relaxed);
            }
            "close" => {
                let _ = close_proxy.send_event(DesktopEvent::Close);
            }
            "open_folder" => {
                let _ = std::process::Command::new("explorer").arg(&dir_ipc).spawn();
            }
            _ => {}
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(DesktopEvent::Show) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
            Event::UserEvent(DesktopEvent::Quit) => {
                quit.notify_one();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(DesktopEvent::Close)
            | Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if cfg!(windows)
                    && crate::desktop::load(&data_dir)
                        .unwrap_or_default()
                        .close_to_tray
                {
                    window.set_visible(false);
                } else {
                    quit.notify_one();
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::LoopDestroyed => {
                #[cfg(windows)]
                drop(tray.take());
            }
            _ => {}
        }
    });
}
