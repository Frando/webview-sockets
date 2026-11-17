//! Dev server for pages using webview-sockets (`UDPSocket`) without an app.
//!
//! Serves a directory on 127.0.0.1 and mounts the UDP route. It also serves
//! the injected script at `/.well-known/webview-sockets/polyfill.js`, with a
//! dev adapter that gets sessions from
//! `/.well-known/webview-sockets/dev-session`. Pages load the script when
//! `window.webviewSockets` is missing. It defines `UDPSocket` unless the
//! browser has a native one:
//!
//! ```html
//! <script src="/.well-known/webview-sockets/polyfill.js"></script>
//! ```
//!
//! Run: `cargo run -p webview-sockets-host --example test-host -- --dir <dir>
//! [--port N] [--auto-grant]` and open `http://test.localhost:<port>/`.

use std::{net::SocketAddr, path::PathBuf};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use n0_error::{Result, StdResultExt};
use tower_http::services::ServeDir;
use tracing::{info, warn};
use webview_sockets_host::{
    Choice, Decision, HostPolicy, SessionTicket, TabId, UdpHost, UdpHostConfig, bridge_reply,
};

/// Script added before the injected script to get sessions from this server.
///
/// The injected script uses it when the page has no platform bridge.
const DEV_ADAPTER: &str = r#"window.__webviewSocketsDevSession = async () => {
  const response = await fetch("/.well-known/webview-sockets/dev-session", { method: "POST" });
  return await response.json();
};
"#;

/// Command line arguments.
#[derive(Parser, Debug)]
#[command(about = "Serve a directory with the webview-sockets host for testing")]
struct Args {
    /// Directory to serve.
    #[arg(long)]
    dir: PathBuf,
    /// Port on 127.0.0.1 (0 picks a free one).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Grant every origin without asking (allow once, tab 0).
    #[arg(long)]
    auto_grant: bool,
    /// Grant store file (default: a fresh file in the temp dir).
    #[arg(long)]
    store: Option<PathBuf>,
    /// Accept any Host, not only `<name>.localhost:<port>`.
    #[arg(long)]
    any_host: bool,
}

/// State shared by the HTTP handlers.
#[derive(Clone)]
struct AppState {
    host: UdpHost,
    /// Dev adapter plus the injected script.
    script: String,
    auto_grant: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,webview_sockets_host=debug".into()),
        )
        .init();
    let args = Args::parse();
    let store = args.store.unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "webview-sockets-test-host-{}.json",
            std::process::id()
        ))
    });
    let policy = if args.any_host {
        HostPolicy::Any
    } else {
        HostPolicy::patterns(["*.localhost:*"])
    };
    let host = UdpHost::new(UdpHostConfig::new(&store, policy))?;
    host.set_enabled(true);

    let state = AppState {
        script: format!("{DEV_ADAPTER}{}", host.injected_js()),
        host: host.clone(),
        auto_grant: args.auto_grant,
    };
    let app = Router::new()
        .route("/.well-known/webview-sockets/polyfill.js", get(host_js))
        .route(
            "/.well-known/webview-sockets/dev-session",
            post(dev_session),
        )
        .with_state(state)
        .merge(host.router())
        .fallback_service(ServeDir::new(&args.dir));

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], args.port)))
        .await
        .std_context("bind")?;
    let port = listener.local_addr().std_context("local addr")?.port();
    info!(dir = %args.dir.display(), store = %store.display(), auto_grant = args.auto_grant, "serving");
    println!("test-host listening on 127.0.0.1:{port}");
    println!("open http://test.localhost:{port}/");
    axum::serve(listener, app).await.std_context("serve")?;
    Ok(())
}

/// Serves the dev adapter and the injected script.
async fn host_js(State(state): State<AppState>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        state.script,
    )
}

/// Answers a session request from the dev adapter, like a platform bridge.
async fn dev_session(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let header_value = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let origin = match (header_value(header::ORIGIN), header_value(header::HOST)) {
        (Some(origin), _) => origin.to_string(),
        (None, Some(host)) => format!("http://{host}"),
        (None, None) => return (StatusCode::BAD_REQUEST, "no origin".to_string()),
    };
    let tab = TabId(0);
    let result: Result<SessionTicket, &str> = match state.host.request(&origin, tab) {
        Decision::Granted(ticket) => Ok(ticket),
        Decision::NeedPrompt if state.auto_grant => state
            .host
            .answer(&origin, tab, Choice::AllowOnce)
            .ok_or("NotAllowed"),
        Decision::NeedPrompt => {
            warn!(%origin, "udp session denied, run with --auto-grant to allow it");
            Err("NotAllowed")
        }
        Decision::Denied => Err("NotAllowed"),
        Decision::Disabled => Err("Disabled"),
    };
    info!(%origin, ok = result.is_ok(), "dev session");
    (
        StatusCode::OK,
        bridge_reply(0, result.as_ref().map_err(|reason| *reason)),
    )
}
