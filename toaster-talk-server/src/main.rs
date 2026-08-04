use arc_swap::ArcSwap;
use clap::Parser;
use std::sync::Arc;
use toaster_talk_server::{
    config::{CliArgs, Config},
    http_transport::map_session_401_to_404,
    mgmt,
    rooms::RoomStore,
    server::ToasterServer,
    state::AppState,
    ttl,
};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Resolve the active log filter: `RUST_LOG` wins if set (fixed for the process
/// lifetime), otherwise the config's `log_level`. Used both at startup and on every
/// reload so the rule stays identical.
fn make_filter(cfg: &Config) -> EnvFilter {
    EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| cfg.log_level.parse().unwrap_or_default())
}

/// Re-resolve config from disk and swap it into the live cell so running handlers,
/// the sweeper, and the mgmt listener pick up the new tunables without dropping a
/// single connection. Restart-only fields (bind/port/socket/store/secret) are
/// retained by `Config::reload`, which warns on any attempt to change them. A failed
/// reload (e.g. a malformed edit) is logged and the running config is left intact.
///
/// Also re-reads `rooms.yaml` (the room store), so an operator's hand-edit — for
/// example extending a room's or seat's `expires_at_secs` — takes effect without a
/// restart. `store_path` itself stays fixed (restart-only); only its contents are
/// re-read. A missing or corrupt file leaves the running store intact, exactly like
/// a malformed config edit.
///
/// Shared by every reload trigger — `SIGHUP` on Unix (`run_sighup_reloader`) and the
/// mgmt `RELOAD` command on every platform (`run_command_reloader`) — so a reload
/// behaves identically no matter what triggered it (TDD 8.13–8.17).
async fn apply_reload<H>(
    args: &CliArgs,
    startup: &Config,
    live: &ArcSwap<Config>,
    store: &RwLock<RoomStore>,
    log_reload: &H,
) where
    H: Fn(EnvFilter),
{
    match Config::reload(startup, args) {
        Ok(new_cfg) => {
            let new_cfg = Arc::new(new_cfg);
            log_reload(make_filter(&new_cfg));
            live.store(new_cfg);
            info!("configuration reloaded");
        }
        Err(e) => {
            error!(error = %e, "configuration reload failed — keeping previous config");
        }
    }

    match RoomStore::reload(startup.store_path.clone()) {
        Ok(new_store) => {
            *store.write().await = new_store;
            info!("room store reloaded");
        }
        Err(e) => {
            error!(error = %e, "room store reload failed — keeping previous store");
        }
    }
}

/// Background task: on each `SIGHUP`, apply a reload — this is what backs
/// `systemctl reload`. Unix only; Windows has no `SIGHUP` and relies solely on
/// `run_command_reloader` below (armed on every platform).
#[cfg(unix)]
async fn run_sighup_reloader<H>(
    args: Arc<CliArgs>,
    startup: Arc<Config>,
    live: Arc<ArcSwap<Config>>,
    store: Arc<RwLock<RoomStore>>,
    log_reload: H,
) where
    H: Fn(EnvFilter),
{
    use tokio::signal::unix::{signal, SignalKind};

    let mut hangups = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "could not install SIGHUP handler — signal-triggered reload \
                disabled (the mgmt RELOAD command still works)");
            return;
        }
    };
    info!("SIGHUP config reload armed (systemctl reload)");

    while hangups.recv().await.is_some() {
        info!("SIGHUP received — reloading configuration");
        apply_reload(&args, &startup, &live, &store, &log_reload).await;
    }
}

/// Background task: applies a reload whenever the mgmt `RELOAD` command (TDD 8.17)
/// triggers one. Armed on every platform — the only reload path on Windows, and an
/// explicit alternative to `SIGHUP` on Unix. Each trigger carries a completion
/// channel so the mgmt connection handler replies to the operator only once the
/// reload has actually been applied, not merely queued.
async fn run_command_reloader<H>(
    args: Arc<CliArgs>,
    startup: Arc<Config>,
    live: Arc<ArcSwap<Config>>,
    store: Arc<RwLock<RoomStore>>,
    mut triggers: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    log_reload: H,
) where
    H: Fn(EnvFilter),
{
    info!("mgmt RELOAD command armed");
    while let Some(ack) = triggers.recv().await {
        info!("RELOAD command received — reloading configuration");
        apply_reload(&args, &startup, &live, &store, &log_reload).await;
        let _ = ack.send(());
    }
}

/// The server body, extracted from `main` so it can be driven either directly
/// (Unix: a plain `#[tokio::main]`) or from inside a Windows service, whose
/// Service Control Manager `Stop`/`Shutdown` events resolve `shutdown_signal` to
/// drain the MCP HTTP server gracefully — the Windows equivalent of what `systemd`
/// gives the Unix build via `SIGTERM` for free. Unix's own `main` passes a
/// `pending()` future here (never resolves), leaving its behavior unchanged.
async fn run(shutdown_signal: impl std::future::Future<Output = ()> + Send + 'static) {
    let args = Arc::new(CliArgs::parse());

    // ── Config ────────────────────────────────────────────────────────────────
    // `startup` is the immutable baseline; `live` is the cell a SIGHUP reload swaps.
    let startup = match Config::load(&args) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };
    let live: Arc<ArcSwap<Config>> = Arc::new(ArcSwap::new(startup.clone()));

    // ── Logging ───────────────────────────────────────────────────────────────
    // Built from a reloadable filter layer so a SIGHUP can retune `log_level`.
    let (filter_layer, reload_handle) =
        tracing_subscriber::reload::Layer::new(make_filter(&startup));
    {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(filter_layer)
            .with(tracing_subscriber::fmt::layer())
            .init();
    }

    info!(
        bind = %startup.bind,
        port = startup.port,
        "toaster-talk-server starting"
    );

    // ── Room store ────────────────────────────────────────────────────────────
    let store = match RoomStore::load(startup.store_path.clone()) {
        Ok(s) => Arc::new(RwLock::new(s)),
        Err(e) => {
            error!(error = %e, "failed to load room store");
            std::process::exit(1);
        }
    };

    // ── In-memory state ───────────────────────────────────────────────────────
    let app = AppState::new();

    // ── Config reloaders (SIGHUP on Unix, mgmt RELOAD command everywhere) ────────
    let (reload_tx, reload_rx) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    {
        #[cfg(unix)]
        {
            let reload_handle = reload_handle.clone();
            tokio::spawn(run_sighup_reloader(
                args.clone(),
                startup.clone(),
                live.clone(),
                store.clone(),
                move |filter| {
                    let _ = reload_handle.modify(|f| *f = filter);
                },
            ));
        }
        let reload_handle = reload_handle.clone();
        tokio::spawn(run_command_reloader(
            args.clone(),
            startup.clone(),
            live.clone(),
            store.clone(),
            reload_rx,
            move |filter| {
                let _ = reload_handle.modify(|f| *f = filter);
            },
        ));
    }

    // ── TTL sweeper ───────────────────────────────────────────────────────────
    let sweeper_handle = tokio::spawn(ttl::run_sweeper(app.clone(), store.clone(), live.clone()));

    // ── Management socket/pipe ────────────────────────────────────────────────
    let mgmt_handle = tokio::spawn(mgmt::run_mgmt_listener(
        startup.mgmt_socket.clone(),
        store.clone(),
        app.clone(),
        live.clone(),
        reload_tx,
    ));

    // ── MCP HTTP server (dual transport) ─────────────────────────────────────
    let bind_addr = format!("{}:{}", startup.bind, startup.port);

    // Streamable HTTP transport — MCP 2025-03-26 spec (POST /mcp)
    // Used by Kiro and any host implementing the newer spec.
    //
    // Runs in the default **stateful** mode: the transport retains a session per
    // `Mcp-Session-Id`, which is the channel a server-initiated push (standalone
    // GET /mcp SSE stream) would ride. We keep it — the post-restart "stale session
    // hang" is fixed not by discarding sessions but by `map_session_401_to_404`
    // below, which aligns this transport with the classic SSE transport's existing
    // behaviour. See TDD 1.6 and ANTI-PATTERNS #9.
    let streamable_service = {
        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        };
        let handler_config = live.clone();
        let handler_store = store.clone();
        let handler_app = app.clone();
        StreamableHttpService::new(
            move || {
                Ok(ToasterServer::with_live_config(
                    handler_app.clone(),
                    handler_store.clone(),
                    handler_config.clone(),
                ))
            },
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default(),
        )
    };

    // Classic SSE transport — MCP 2024-11-05 spec (GET /sse + POST /message)
    // Used by Junie and any host implementing the older spec. POST /message
    // returns 202 immediately so the host HTTP client has no long-lived request
    // to time out against — this resolves ISS-005 for SSE-capable hosts.
    let sse_router = {
        use rmcp::transport::sse_server::{SseServer, SseServerConfig};
        use tokio_util::sync::CancellationToken;
        let handler_config = live.clone();
        let handler_store = store.clone();
        let handler_app = app.clone();
        let sse_config = SseServerConfig {
            bind: bind_addr.parse().unwrap_or_else(|_| {
                error!(addr = %bind_addr, "invalid bind address");
                std::process::exit(1);
            }),
            sse_path: "/sse".to_string(),
            post_path: "/message".to_string(),
            ct: CancellationToken::new(),
            sse_keep_alive: None,
        };
        let (sse_server, sse_router) = SseServer::new(sse_config);
        sse_server.with_service(move || {
            ToasterServer::with_live_config(
                handler_app.clone(),
                handler_store.clone(),
                handler_config.clone(),
            )
        });
        sse_router
    };

    let mcp_router = axum::Router::new()
        .route_service("/mcp", streamable_service)
        .layer(axum::middleware::map_response(map_session_401_to_404));

    let router = mcp_router.merge(sse_router);

    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr = %bind_addr, error = %e, "failed to bind MCP listener");
            std::process::exit(1);
        }
    };

    info!(addr = %bind_addr, streamable_http = "/mcp", sse = "/sse", "MCP server listening");

    tokio::select! {
        result = axum::serve(listener, router).with_graceful_shutdown(shutdown_signal) => {
            if let Err(e) = result {
                error!(error = %e, "MCP server error");
                std::process::exit(1);
            }
        }
        result = sweeper_handle => {
            match result {
                Ok(()) => error!("TTL sweeper task exited unexpectedly"),
                Err(e) => error!(error = %e, "TTL sweeper task panicked"),
            }
            std::process::exit(1);
        }
        result = mgmt_handle => {
            match result {
                Ok(Ok(())) => error!("management socket task exited unexpectedly"),
                Ok(Err(reason)) => error!(%reason, "management socket setup failed"),
                Err(e) => error!(error = %e, "management socket task panicked"),
            }
            std::process::exit(1);
        }
    }

    info!("shutdown complete");
}

#[cfg(unix)]
#[tokio::main]
async fn main() {
    // No SCM equivalent to wait on — Unix's shutdown story stays exactly what it
    // was before this function existed.
    run(std::future::pending()).await;
}

#[cfg(windows)]
fn main() {
    windows_service_support::run_dual_mode();
}

/// Hosts the daemon as a native Windows service when launched by the Service
/// Control Manager, falling back to a plain foreground process (identical to the
/// Unix build's behavior) for every other invocation — `cargo run`, a manual
/// double-click, or a debugging session. `service_dispatcher::start` only
/// succeeds when the SCM actually spawned this process; any other caller gets
/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`, the documented signal to fall back.
#[cfg(windows)]
mod windows_service_support {
    use crate::run;
    use std::{ffi::OsString, sync::mpsc as std_mpsc, time::Duration};
    use tracing::error;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    const SERVICE_NAME: &str = "ToasterTalk";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    // Returned by StartServiceCtrlDispatcherW when the calling process was not
    // actually launched by the Service Control Manager — the documented signal to
    // run as a normal console process instead.
    const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

    define_windows_service!(ffi_service_main, service_main);

    pub fn run_dual_mode() {
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => {}
            Err(windows_service::Error::Winapi(e))
                if e.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) =>
            {
                run_console();
            }
            Err(e) => {
                eprintln!("could not start toaster-talk-server as a Windows service: {e}");
                std::process::exit(1);
            }
        }
    }

    fn run_console() {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(run(std::future::pending()));
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service() {
            // The tracing subscriber isn't initialized yet if this failed before
            // `run` got to its logging setup — eprintln! is the only guaranteed
            // sink at this point, so emit both.
            eprintln!("windows service failed: {e}");
            error!(error = %e, "windows service failed");
        }
    }

    fn run_service() -> windows_service::Result<()> {
        let (scm_shutdown_tx, scm_shutdown_rx) = std_mpsc::channel::<()>();

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = scm_shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        // Bridge the SCM's std::sync::mpsc control-event channel into an
        // async-friendly future `run` can race against in its own select!.
        let (async_shutdown_tx, async_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            let _ = scm_shutdown_rx.recv();
            let _ = async_shutdown_tx.send(());
        });

        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(run(async move {
            let _ = async_shutdown_rx.await;
        }));

        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }
}
