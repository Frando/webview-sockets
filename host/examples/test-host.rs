//! Dev server for pages using `window.irohHost.udp` without an app.
//!
//! Serves a directory on 127.0.0.1 for any Host, mounts the UDP route, and
//! serves the injected script at `/.well-known/iroh-link/host.js` with a dev
//! adapter that gets sessions from `/.well-known/iroh-link/dev-session`.
//! Pages load it when `window.irohHost` is missing:
//!
//! ```html
//! <script src="/.well-known/iroh-link/host.js"></script>
//! ```
//!
//! Run: `cargo run -p iroh-link-udp-host --example test-host -- --dir <dir>
//! [--port N] [--auto-grant]` and open `http://test.pkarr.localhost:<port>/`.

use std::{net::SocketAddr, path::PathBuf};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use iroh_link_udp_host::{
    Choice, Decision, INJECTED_JS, SessionTicket, TabId, UdpHost, UdpHostConfig, bridge_reply,
};
use n0_error::{Result, StdResultExt};
use tower_http::services::ServeDir;
use tracing::{info, warn};

/// Prepended to the injected script: asks this server for a session when
/// the page has no platform bridge.
const DEV_ADAPTER: &str = r#"window.__irohHostDevSession = async () => {
  const r = await fetch("/.well-known/iroh-link/dev-session", { method: "POST" });
  return await r.json();
};
"#;

#[derive(Parser, Debug)]
#[command(about = "Serve a directory with the iroh-link UDP host for testing")]
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
    /// Accept any Host, not only `<label>.(blake3|pkarr).localhost:<port>`.
    #[arg(long)]
    any_host: bool,
}

#[derive(Clone)]
struct AppState {
    host: UdpHost,
    auto_grant: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh_link_udp_host=debug".into()),
        )
        .init();
    let args = Args::parse();
    let store = args.store.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("iroh-link-test-host-{}.json", std::process::id()))
    });
    let mut config = UdpHostConfig::new(&store);
    config.check_host = !args.any_host;
    let host = UdpHost::new(config)?;
    host.set_enabled(true);

    let state = AppState {
        host: host.clone(),
        auto_grant: args.auto_grant,
    };
    let app = Router::new()
        .route("/.well-known/iroh-link/host.js", get(host_js))
        .route("/.well-known/iroh-link/dev-session", post(dev_session))
        .with_state(state)
        .merge(host.router())
        .fallback_service(ServeDir::new(&args.dir));

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], args.port)))
        .await
        .std_context("bind")?;
    let port = listener.local_addr().std_context("local addr")?.port();
    info!(dir = %args.dir.display(), store = %store.display(), auto_grant = args.auto_grant, "serving");
    println!("test-host listening on 127.0.0.1:{port}");
    println!("open http://test.pkarr.localhost:{port}/");
    axum::serve(listener, app).await.std_context("serve")?;
    Ok(())
}

async fn host_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("{DEV_ADAPTER}{INJECTED_JS}"),
    )
}

async fn dev_session(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let get = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let origin = match (get(header::ORIGIN), get(header::HOST)) {
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
            warn!(%origin, "udp session requested; run with --auto-grant to allow");
            Err("NotAllowed")
        }
        Decision::Denied => Err("NotAllowed"),
        Decision::Disabled => Err("Disabled"),
    };
    info!(%origin, ok = result.is_ok(), "dev session");
    (
        StatusCode::OK,
        bridge_reply(0, result.as_ref().map_err(|e| *e)),
    )
}
