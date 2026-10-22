#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appearance;
mod handlers;
mod state;
mod static_assets;
mod window;
mod desktop;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, Path},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use mg_core::{AccountPool, Activity, Config, KeyManager};
use mg_providers::{
    AntigravityProvider, CopilotProvider, GrokProvider, MiniMaxProvider, Provider, QoderProvider,
    TraeProvider, WorkBuddyProvider,
};
use parking_lot::RwLock;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::state::AppState;
use crate::static_assets::{if_none_match, serve_asset};

/// Agent conversation histories and base64 image inputs routinely exceed Axum's
/// 2MB default, which surfaced to clients as an opaque 413.
const MAX_INFERENCE_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(author, version, about = "High Performance Multi-Account AI Reverse Proxy Gateway in Rust")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    #[arg(long, default_value_t = 8791)]
    port: u16,

    #[arg(long)]
    data_dir: Option<PathBuf>,

    #[arg(long)]
    api_key: Option<String>,

    #[arg(long, default_value_t = false)]
    headless: bool,

    /// Start in the system tray without showing the desktop window.
    #[arg(long, default_value_t = false)]
    silent: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    #[cfg(windows)]
    if args.headless {
        unsafe {
            // Attach to parent console if running in headless mode from terminal
            #[link(name = "kernel32")]
            extern "system" {
                fn AttachConsole(dwProcessId: u32) -> i32;
            }
            AttachConsole(0xFFFFFFFF);
        }
    }

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let config = Config::load_from_env_and_args(
        Some(args.host),
        Some(args.port),
        args.data_dir,
        args.api_key,
        args.headless,
    );

    config.ensure_dirs()?;

    let activity = Arc::new(Activity::new(&config.data_dir));
    let key_manager = Arc::new(KeyManager::new(&config.data_dir, config.api_key.clone())?);

    // Initialize Providers (WorkBuddy, Trae, MiniMax, Qoder, Antigravity, Copilot, Grok)
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    // Management handlers must mutate the very same pools the providers route
    // through; a freshly-scanned pool would let a disabled or deleted account
    // keep serving traffic until the process restarts.
    let mut pools: HashMap<String, Arc<AccountPool>> = HashMap::new();

    let wb_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "workbuddy")));
    pools.insert("workbuddy".to_string(), wb_pool.clone());
    let workbuddy_home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let intl_model_path = workbuddy_home
        .as_ref()
        .map(|home| home.join(".workbuddy-ai/cache/acc-product-config-v3.json"));
    let cn_model_path = workbuddy_home
        .as_ref()
        .map(|home| home.join(".workbuddy/cache/acc-product-config-v3.json"));
    let legacy_model_path = Some(
        config
            .data_dir
            .join("workbuddy")
            .join("accounts")
            .join("model.json"),
    );
    let workbuddy = Arc::new(WorkBuddyProvider::new(
        wb_pool,
        intl_model_path,
        cn_model_path,
        legacy_model_path,
    ));
    providers.insert("workbuddy".to_string(), workbuddy);

    let trae_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "trae")));
    pools.insert("trae".to_string(), trae_pool.clone());
    let trae = Arc::new(TraeProvider::new(trae_pool));
    providers.insert("trae".to_string(), trae);

    let qoder_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "qoder")));
    pools.insert("qoder".to_string(), qoder_pool.clone());
    let qoder = Arc::new(QoderProvider::new(qoder_pool));
    providers.insert("qoder".to_string(), qoder);

    let antigravity_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "antigravity")));
    pools.insert("antigravity".to_string(), antigravity_pool.clone());
    let antigravity = Arc::new(AntigravityProvider::new(
        antigravity_pool,
        config.antigravity_client_id.clone(),
        config.antigravity_client_secret.clone(),
    ));
    providers.insert("antigravity".to_string(), antigravity);

    let copilot_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "copilot")));
    pools.insert("copilot".to_string(), copilot_pool.clone());
    let copilot = Arc::new(CopilotProvider::new(copilot_pool));
    providers.insert("copilot".to_string(), copilot);

    let grok_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "grok")));
    pools.insert("grok".to_string(), grok_pool.clone());
    let grok = Arc::new(GrokProvider::new(grok_pool));
    providers.insert("grok".to_string(), grok);

    let minimax_pool = Arc::new(AccountPool::new(mg_core::resolve_pool_dir(&config.data_dir, "minimax")));
    pools.insert("minimax".to_string(), minimax_pool.clone());
    let minimax = Arc::new(MiniMaxProvider::new(minimax_pool));
    providers.insert("minimax".to_string(), minimax);

    let state = AppState {
        config: Arc::new(config.clone()),
        activity,
        key_manager,
        providers: Arc::new(RwLock::new(providers)),
        pools: Arc::new(pools),
    };

    // Permissive CORS is confined to the key-authenticated inference API, which
    // browser-based clients legitimately call cross-origin. The management surface
    // stays same-origin (no CORS headers) so that a web page the operator visits
    // cannot read API keys or account data from http://127.0.0.1:<port>.
    let inference_cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let inference = Router::new()
        // OpenAI / Anthropic inference routes
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/responses", post(handlers::responses))
        .route("/v1/messages", post(handlers::messages))
        .route("/v1/models", get(handlers::list_models))
        .route("/models", get(handlers::list_models))
        .route("/health", get(|| async {
            Json(serde_json::json!({
                "ok": true,
                "application": "MultiGateway-Rust",
                "version": env!("CARGO_PKG_VERSION")
            }))
        }))
        // Grouped provider prefix routes
        .route("/:provider/v1/chat/completions", post(handlers::provider_chat_completions))
        .route("/:provider/v1/messages", post(handlers::provider_messages))
        .route("/:provider/v1/responses", post(handlers::provider_responses))
        .route("/:provider/v1/models", get(handlers::list_provider_models))
        // Agent contexts and base64 image inputs routinely exceed Axum's 2MB default.
        .layer(DefaultBodyLimit::max(MAX_INFERENCE_BODY_BYTES))
        .layer(inference_cors);

    let dashboard = Router::new()
        // Dashboard and static assets
        .route("/", get(|headers: HeaderMap| async move {
            serve_asset("index.html", if_none_match(&headers)).await
        }))
        .route("/favicon.ico", get(|headers: HeaderMap| async move {
            serve_asset("favicon.ico", if_none_match(&headers)).await
        }))
        .route("/gateway/ui/*path", get(|headers: HeaderMap, Path(path): Path<String>| async move {
            serve_asset(&path, if_none_match(&headers)).await
        }))
        // Gateway management APIs
        .route("/gateway/status", get(handlers::gateway_status))
        .route("/gateway/usage/board", get(handlers::usage_board))
        .route(
            "/gateway/providers/:provider/snapshot",
            get(handlers::provider_snapshot),
        )
        .route(
            "/gateway/providers/:provider/models",
            get(handlers::provider_models),
        )
        .route(
            "/gateway/providers/:provider/login",
            post(handlers::provider_login),
        )
        .route(
            "/gateway/providers/:provider/poll",
            post(handlers::provider_poll),
        )
        .route(
            "/gateway/providers/:provider/enable",
            post(handlers::provider_enable_account),
        )
        .route(
            "/gateway/providers/:provider/delete",
            post(handlers::provider_delete_account),
        )
        .route(
            "/gateway/providers/:provider/credits",
            post(handlers::provider_credits),
        )
        .route(
            "/gateway/providers/:provider/credentials",
            post(handlers::provider_credentials_action),
        )
        .route(
            "/gateway/providers/:provider/checkin",
            post(handlers::provider_checkin_action),
        )
        .route(
            "/gateway/providers/:provider/batch",
            post(handlers::provider_batch),
        )
        .route(
            "/gateway/providers/:provider/region",
            post(handlers::provider_region),
        )
        .route(
            "/gateway/providers/:provider/import",
            post(handlers::provider_import),
        )
        .route("/gateway/configure", post(handlers::gateway_configure))
        // Settings & Keys APIs
        .route("/settings", get(handlers::settings_view))
        .route("/settings/desktop", post(handlers::desktop_settings_save))
        .route("/settings/appearance", post(handlers::appearance_settings_save))
        .route("/settings/reveal", get(handlers::settings_reveal))
        .route(
            "/gateway/api-keys/generate",
            post(handlers::api_key_generate),
        )
        .route("/gateway/api-keys/delete", post(handlers::api_key_delete))
        .route("/gateway/api-keys/list", get(handlers::api_key_list))
        .route("/gateway/api-keys", get(handlers::api_key_list))
        // The dashboard ships ~50KB of CSS/JS and the usage board returns large JSON;
        // SSE lives only on the inference router, so compression is safe here.
        .layer(CompressionLayer::new());

    let app = Router::new()
        .merge(dashboard)
        .merge(inference)
        .fallback(handlers::fallback_404)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;

    // Create a multi-threaded Tokio runtime for the background HTTP server
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
    // The desktop window signals this when the user quits so in-flight streams can
    // drain instead of being cut off by the runtime dropping.
    let quit = Arc::new(tokio::sync::Notify::new());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    let server_quit = quit.clone();
    rt.spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                let _ = ready_tx.send(Ok(()));
                info!("🚀 MultiGateway Rust is running on http://{}", addr);
                let shutdown = async move {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {},
                        _ = server_quit.notified() => {},
                    }
                    info!("Shutdown signal received, draining in-flight requests...");
                };
                if let Err(e) = axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown)
                    .await
                {
                    error!("HTTP server stopped with error: {}", e);
                }
                let _ = done_tx.send(());
            }
            Err(e) => {
                error!("Failed to bind server on {}: {}", addr, e);
                let _ = ready_tx.send(Err(std::io::Error::new(
                    e.kind(),
                    format!("Failed to bind server on {}: {}", addr, e),
                )));
                let _ = done_tx.send(());
            }
        }
    });

    // Do not open a second desktop window if another instance already owns the port.
    ready_rx
        .recv()
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Server startup task stopped",
            )
        })??;

    println!("\n=======================================================");
    println!("  MultiGateway Rust (v{}) Desktop Application", env!("CARGO_PKG_VERSION"));
    println!("  Dashboard Web UI: http://{}:{}", config.host, config.port);
    println!("  OpenAI API Base:  http://{}:{}/v1", config.host, config.port);
    println!("  Data Directory:   {}", config.data_dir.display());
    println!("=======================================================\n");

    if config.headless {
        info!("Running in headless mode. Press Ctrl+C to terminate.");
        // Park main thread while Tokio background server runs
        loop {
            std::thread::park();
        }
    } else {
        // Run Desktop WebView window on the main thread
        info!("Launching Desktop Window...");
        // A corrupt desktop-settings.json must not stop the gateway from booting;
        // fall back to defaults like the window path already does.
        let settings = desktop::load(&config.data_dir).unwrap_or_else(|error| {
            error!("Desktop settings unreadable ({}), using defaults", error);
            desktop::DesktopSettings::default()
        });
        window::run_desktop_window(
            config.port,
            config.data_dir,
            args.silent || settings.start_silently,
            quit,
        )?;
        // Let in-flight streams finish before the runtime is dropped; bounded so a
        // stuck upstream cannot hang process exit.
        let _ = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), done_rx).await
        });
    }

    Ok(())
}
