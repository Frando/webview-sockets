//! Host side of webview-sockets: native UDP sockets for pages in a webview.
//!
//! An app embeds a webview and serves its pages over HTTP from a local
//! server. A page on an origin the app allows (see [`HostPolicy`]) asks the
//! platform bridge for a session ticket ([`UdpHost::request`],
//! [`UdpHost::answer`]), then opens one WebSocket per UDP socket on
//! [`WS_PATH`] of its own origin ([`UdpHost::router`]). The wire format is in
//! [`webview_sockets_proto`].
//!
//! The script injected into pages ([`UdpHost::injected_js`], [`INJECTED_JS`])
//! gives them `webviewSockets.udp` and a Direct Sockets `UDPSocket` layer:
//! sockets bound to a local address, or connected to a remote host name or
//! address that the host resolves. Its
//! permission step asks the platform bridge ([`BridgeRequest`],
//! [`bridge_reply`], [`reply_script`]).
//!
//! Access is off by default ([`UdpHost::set_enabled`]) and granted per
//! origin, either always (persisted) or once (until the tab navigates or
//! closes).

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use tracing::{debug, info, warn};
use webview_sockets_proto::{TOKEN_LEN, close_code};

mod bridge;
mod js;
mod policy;
mod session;
mod sock;
mod store;

pub use bridge::{BridgeRequest, bridge_reply, reply_script};
pub use js::{INJECTED_JS, injected_js_for};
pub use policy::HostPolicy;
pub use webview_sockets_proto::WS_PATH;

/// Identifies a browser tab, as chosen by the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

/// Resource limits for pages using UDP.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum concurrent sockets per origin, across all tabs.
    pub max_sockets_per_origin: usize,
    /// Maximum concurrent sockets per session token.
    pub max_sockets_per_token: usize,
    /// Maximum size of one WebSocket message in bytes.
    pub max_message_len: usize,
    /// Number of received WebSocket messages (each up to 64 KiB) queued per
    /// socket before further datagrams are dropped.
    pub recv_queue_len: usize,
    /// Time the page has to send HELLO after the WebSocket opened.
    pub hello_timeout: Duration,
    /// Time allowed for resolving a host name.
    pub resolve_timeout: Duration,
    /// Upper bound for the kernel send and receive buffer sizes a page may
    /// request; also the size used when the page requests none.
    pub max_socket_buffer: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sockets_per_origin: 8,
            max_sockets_per_token: 8,
            max_message_len: webview_sockets_proto::MAX_MESSAGE_LEN,
            recv_queue_len: 64,
            hello_timeout: Duration::from_secs(5),
            resolve_timeout: Duration::from_secs(10),
            max_socket_buffer: 2 * 1024 * 1024,
        }
    }
}

/// Configuration for [`UdpHost::new`].
#[derive(Debug, Clone)]
pub struct UdpHostConfig {
    /// JSON file holding the enabled flag and the per-origin grants.
    pub store_path: PathBuf,
    /// Resource limits.
    pub limits: Limits,
    /// Which page hosts may use sockets.
    pub host_policy: HostPolicy,
}

impl UdpHostConfig {
    /// Returns a config with default limits.
    pub fn new(store_path: impl Into<PathBuf>, host_policy: HostPolicy) -> Self {
        Self {
            store_path: store_path.into(),
            limits: Limits::default(),
            host_policy,
        }
    }
}

/// Result of [`UdpHost::request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The origin may use UDP; hand the ticket to the page.
    Granted(SessionTicket),
    /// Ask the user, then call [`UdpHost::answer`].
    NeedPrompt,
    /// The user blocked the origin, or the [`HostPolicy`] does not allow it.
    Denied,
    /// The UDP setting is off.
    Disabled,
}

/// The user's answer to a UDP prompt, see [`UdpHost::answer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Allow the origin now and in future (persisted).
    AllowAlways,
    /// Allow the origin in this tab until it navigates or closes.
    AllowOnce,
    /// Block the origin (persisted).
    Block,
}

/// A persisted per-origin decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grant {
    /// The origin may use UDP.
    Allow,
    /// The origin may not use UDP and is not prompted again.
    Block,
}

/// What the bridge returns to the page: where to connect and with which token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionTicket {
    /// WebSocket URL, the page origin with [`WS_PATH`].
    pub url: String,
    /// Session token as 64 lowercase hex chars.
    pub token: String,
}

/// Host side of native UDP for web pages.
///
/// Cheap to clone. All methods are synchronous and may be called from any
/// thread; [`UdpHost::router`] must run on a tokio runtime.
#[derive(Debug, Clone)]
pub struct UdpHost {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: UdpHostConfig,
    state: Mutex<State>,
    active: watch::Sender<BTreeSet<TabId>>,
}

type Token = [u8; TOKEN_LEN];

#[derive(Debug)]
struct State {
    stored: store::Stored,
    /// "Allow once" grants, scoped to a tab.
    once: BTreeSet<(String, TabId)>,
    tickets: HashMap<Token, TicketInfo>,
    sessions: HashMap<u64, SessionEntry>,
    next_session: u64,
}

#[derive(Debug, Clone)]
struct TicketInfo {
    origin: String,
    tab: TabId,
}

#[derive(Debug)]
struct SessionEntry {
    origin: String,
    tab: TabId,
    token: Token,
    close: Option<oneshot::Sender<u16>>,
}

impl UdpHost {
    /// The script to inject into pages, same as [`INJECTED_JS`].
    pub const INJECTED_JS: &'static str = INJECTED_JS;

    /// Creates a host, loading grants and the enabled flag from the store.
    ///
    /// A missing store file means "off, no grants". A corrupt one is logged
    /// and replaced on the next change.
    pub fn new(config: UdpHostConfig) -> n0_error::Result<Self> {
        let stored = store::load(&config.store_path)?;
        let (active, _) = watch::channel(BTreeSet::new());
        let state = State {
            stored,
            once: BTreeSet::new(),
            tickets: HashMap::new(),
            sessions: HashMap::new(),
            next_session: 0,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(state),
                active,
            }),
        })
    }

    /// Returns the script to inject into pages.
    ///
    /// With [`HostPolicy::patterns`] the script only runs on matching hosts
    /// ([`injected_js_for`]); with other policies it is [`INJECTED_JS`] and
    /// runs everywhere, so the app should inject it only into its own pages.
    pub fn injected_js(&self) -> Cow<'static, str> {
        match &self.inner.config.host_policy {
            HostPolicy::Patterns(patterns) => Cow::Owned(injected_js_for(patterns)),
            HostPolicy::Any | HostPolicy::Only(_) => Cow::Borrowed(INJECTED_JS),
        }
    }

    /// Returns the axum router with the WebSocket route on [`WS_PATH`].
    ///
    /// Merge it into the server that serves the app's pages.
    pub fn router(&self) -> axum::Router {
        session::router(self.clone())
    }

    /// Handles a session request from the bridge for a page's origin and tab.
    ///
    /// `origin` must come from the platform (the bridge's reported source
    /// origin), never from the page.
    pub fn request(&self, origin: &str, tab: TabId) -> Decision {
        let mut state = self.inner.lock();
        if !state.stored.enabled {
            return Decision::Disabled;
        }
        if !self.origin_ok(origin) {
            debug!(%origin, "udp request from an origin the host policy denies");
            return Decision::Denied;
        }
        match state.stored.grants.get(origin) {
            Some(Grant::Block) => Decision::Denied,
            Some(Grant::Allow) => Decision::Granted(state.ticket(origin, tab)),
            None if state.once.contains(&(origin.to_string(), tab)) => {
                Decision::Granted(state.ticket(origin, tab))
            }
            None => Decision::NeedPrompt,
        }
    }

    /// Records the user's answer to a prompt and returns a ticket if allowed.
    ///
    /// Returns `None` for [`Choice::Block`], when the setting is off, or for
    /// an origin the [`HostPolicy`] denies.
    pub fn answer(&self, origin: &str, tab: TabId, choice: Choice) -> Option<SessionTicket> {
        if !self.origin_ok(origin) {
            return None;
        }
        let mut state = self.inner.lock();
        if !state.stored.enabled {
            return None;
        }
        info!(%origin, tab = tab.0, ?choice, "udp prompt answered");
        match choice {
            Choice::AllowAlways => {
                state.stored.grants.insert(origin.to_string(), Grant::Allow);
                self.save(&state);
                Some(state.ticket(origin, tab))
            }
            Choice::AllowOnce => {
                state.once.insert((origin.to_string(), tab));
                Some(state.ticket(origin, tab))
            }
            Choice::Block => {
                state.stored.grants.insert(origin.to_string(), Grant::Block);
                self.save(&state);
                state.drop_where(|o, _| o == origin, close_code::REVOKED);
                None
            }
        }
    }

    /// Returns whether the UDP setting is on. Off by default.
    pub fn enabled(&self) -> bool {
        self.inner.lock().stored.enabled
    }

    /// Turns the UDP setting on or off and persists it.
    ///
    /// Turning it off closes all sockets and invalidates all tickets and
    /// "allow once" grants.
    pub fn set_enabled(&self, on: bool) {
        let mut state = self.inner.lock();
        if state.stored.enabled == on {
            return;
        }
        info!(on, "udp setting changed");
        state.stored.enabled = on;
        self.save(&state);
        if !on {
            state.once.clear();
            state.drop_where(|_, _| true, close_code::SETTING_OFF);
        }
    }

    /// Returns the persisted per-origin grants, sorted by origin.
    pub fn grants(&self) -> Vec<(String, Grant)> {
        let state = self.inner.lock();
        state
            .stored
            .grants
            .iter()
            .map(|(o, g)| (o.clone(), *g))
            .collect()
    }

    /// Forgets the grant for `origin` and closes its sockets.
    ///
    /// The origin is prompted again on its next request.
    pub fn revoke(&self, origin: &str) {
        let mut state = self.inner.lock();
        info!(%origin, "udp grant revoked");
        if state.stored.grants.remove(origin).is_some() {
            self.save(&state);
        }
        state.once.retain(|(o, _)| o != origin);
        state.drop_where(|o, _| o == origin, close_code::REVOKED);
    }

    /// Call when the tab's main frame navigates (including reloads).
    ///
    /// Ends "allow once", invalidates the tab's tickets and closes its
    /// sockets.
    pub fn tab_navigated(&self, tab: TabId) {
        self.end_tab(tab);
    }

    /// Call when a tab is closed. Same effect as [`Self::tab_navigated`].
    pub fn tab_closed(&self, tab: TabId) {
        self.end_tab(tab);
    }

    /// Returns a watch of the tabs that currently have open UDP sockets.
    pub fn active_tabs(&self) -> watch::Receiver<BTreeSet<TabId>> {
        self.inner.active.subscribe()
    }

    fn end_tab(&self, tab: TabId) {
        let mut state = self.inner.lock();
        state.once.retain(|(_, t)| *t != tab);
        state.drop_where(|_, t| t == tab, close_code::NAVIGATED);
    }

    fn origin_ok(&self, origin: &str) -> bool {
        policy::origin_ok(origin, &self.inner.config.host_policy)
    }

    fn save(&self, state: &State) {
        if let Err(err) = store::save(&self.inner.config.store_path, &state.stored) {
            warn!(path = %self.inner.config.store_path.display(), "failed to save udp grants: {err:#}");
        }
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("poisoned")
    }

    /// Updates the active tabs watch from the session table.
    fn update_active(&self, state: &State) {
        let tabs: BTreeSet<TabId> = state.sessions.values().map(|s| s.tab).collect();
        self.active.send_if_modified(|cur| {
            if *cur != tabs {
                *cur = tabs;
                true
            } else {
                false
            }
        });
    }
}

impl State {
    /// Returns the ticket for (origin, tab), minting one if needed.
    fn ticket(&mut self, origin: &str, tab: TabId) -> SessionTicket {
        let existing = self
            .tickets
            .iter()
            .find(|(_, t)| t.origin == origin && t.tab == tab)
            .map(|(token, _)| *token);
        let token = existing.unwrap_or_else(|| {
            let token: Token = rand::random();
            self.tickets.insert(
                token,
                TicketInfo {
                    origin: origin.to_string(),
                    tab,
                },
            );
            token
        });
        SessionTicket {
            url: ws_url(origin),
            token: data_encoding::HEXLOWER.encode(&token),
        }
    }

    /// Invalidates matching tickets and closes matching sessions with `code`.
    fn drop_where(&mut self, pred: impl Fn(&str, TabId) -> bool, code: u16) {
        self.tickets.retain(|_, t| !pred(&t.origin, t.tab));
        for s in self.sessions.values_mut() {
            if pred(&s.origin, s.tab)
                && let Some(close) = s.close.take()
            {
                close.send(code).ok();
            }
        }
    }
}

/// Returns the WebSocket URL for a page origin.
fn ws_url(origin: &str) -> String {
    let rest = origin
        .strip_prefix("http://")
        .map(|r| format!("ws://{r}"))
        .or_else(|| {
            origin
                .strip_prefix("https://")
                .map(|r| format!("wss://{r}"))
        })
        .unwrap_or_else(|| origin.to_string());
    format!("{rest}{WS_PATH}")
}
