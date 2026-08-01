use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use futures::{sink::SinkExt, stream::StreamExt};
use serde::Deserialize;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, oneshot};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    pending_sessions: Arc<Mutex<HashMap<String, oneshot::Sender<WebSocket>>>>,
    auth: Arc<AuthConfig>,
}

/// How callers prove they may use this relay. Two independent mechanisms, both
/// optional; a caller passes if it satisfies *either*:
///
/// - `static_token`: one shared secret (`RELAY_TOKEN`) for simple self-hosted
///   setups - no other service needed.
/// - `jwt_key`: Ed25519 *public* key (`RELAY_JWT_PUBLIC_KEY`, PEM file)
///   verifying tokens minted by a kino-control instance. Tokens carry a role
///   and an agent-id scope, so a manager token for host A cannot register
///   agents or reach host B. Verification is asymmetric on purpose: a relay
///   holds no signing material, so in a future federated pool a community
///   relay operator can verify tokens but never mint them.
///
/// With neither set the relay is open, as before - main() warns loudly.
///
/// `jwt_key` is behind an RwLock because self-enrollment installs it *after*
/// the server is already listening (kino-control probes our /healthz before
/// accepting the enrollment, so we must be up first).
struct AuthConfig {
    static_token: Option<String>,
    jwt_key: std::sync::RwLock<Option<jsonwebtoken::DecodingKey>>,
}

/// Claims for kino-control-issued JWTs. `sub` is the role ("agent" or
/// "manager"), `agent_id` scopes the token to one agent id (or "*" for any),
/// and `exp` is checked by the JWT library.
#[derive(Deserialize)]
struct Claims {
    sub: String,
    agent_id: String,
    #[allow(dead_code)]
    exp: usize,
}

#[derive(PartialEq, Clone, Copy)]
enum Role {
    Agent,
    Manager,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Agent => "agent",
            Role::Manager => "manager",
        }
    }
}

/// Admit or reject one WebSocket request, before the upgrade happens.
///
/// `required_agent_id` is the id the caller is trying to act on - the agent id
/// being registered (control) or dialed (manager/request). The data endpoint
/// passes None: its session_id is an unguessable one-shot UUID that only exists
/// after an already-authorized manager request, so possession of a valid token
/// of the right role is enough.
fn authorize(
    auth: &AuthConfig,
    headers: &HeaderMap,
    role: Role,
    required_agent_id: Option<&str>,
) -> Result<(), StatusCode> {
    let jwt_key = auth.jwt_key.read().expect("auth lock poisoned");
    if auth.static_token.is_none() && jwt_key.is_none() {
        return Ok(()); // open mode - warned at startup
    }

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    if let Some(expected) = &auth.static_token {
        use subtle::ConstantTimeEq;
        if bool::from(expected.as_bytes().ct_eq(bearer.as_bytes())) {
            return Ok(());
        }
    }

    if let Some(key) = jwt_key.as_ref() {
        let validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
        if let Ok(data) = jsonwebtoken::decode::<Claims>(bearer, key, &validation) {
            let claims = data.claims;
            let role_ok = claims.sub == role.as_str();
            let scope_ok = match required_agent_id {
                Some(id) => claims.agent_id == "*" || claims.agent_id == id,
                None => true,
            };
            if role_ok && scope_ok {
                return Ok(());
            }
        }
    }

    Err(StatusCode::UNAUTHORIZED)
}

/// A registered agent's control channel. The `token` uniquely identifies this
/// particular connection so a stale connection tearing down can't evict a newer
/// one that reused the same `agent_id`.
struct AgentHandle {
    token: Uuid,
    tx: mpsc::Sender<ControlMessage>,
}

#[derive(serde::Serialize, Clone, Debug)]
#[serde(tag = "type")]
enum ControlMessage {
    #[serde(rename = "new_connection")]
    NewConnection { session_id: String },
}

#[derive(Deserialize)]
struct ControlQuery {
    agent_id: String,
}

#[derive(Deserialize)]
struct ManagerQuery {
    agent_id: String,
}

#[derive(Deserialize)]
struct AgentDataQuery {
    session_id: String,
}

/// Everything needed to self-enroll with a kino-control instance after the
/// server is up. Built from env vars, or interactively when run in a terminal
/// with no auth configured.
struct EnrollPlan {
    control_url: String,
    code: String,
    public_url: String,
    name: Option<String>,
    /// Where to persist the received public key, so restarts skip enrollment
    /// (enrollment codes are one-time).
    key_out: String,
}

/// One blocking enrollment call: register with kino-control, get its public
/// key PEM back.
fn enroll_request(plan: &EnrollPlan) -> Result<String, String> {
    let body = serde_json::json!({
        "code": plan.code,
        "url": plan.public_url,
        "name": plan.name,
    });
    let response = ureq::post(&format!(
        "{}/api/relays/enroll",
        plan.control_url.trim_end_matches('/')
    ))
    .timeout(Duration::from_secs(10))
    .send_json(body)
    .map_err(|e| match e {
        ureq::Error::Status(status, resp) => format!(
            "kino-control rejected enrollment (HTTP {status}): {}",
            resp.into_string().unwrap_or_default()
        ),
        other => format!("cannot reach kino-control: {other}"),
    })?;
    let json: serde_json::Value = response
        .into_json()
        .map_err(|e| format!("bad enrollment response: {e}"))?;
    json["public_key"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "enrollment response had no public_key".to_string())
}

fn prompt(question: &str) -> String {
    use std::io::Write;
    print!("{question}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    line.trim().to_string()
}

/// Ask the operator whether to enroll, when it makes sense to ask: no auth is
/// configured, no env-driven plan exists, and we're attached to a terminal
/// (never block a docker/systemd start on a question nobody will see).
fn interactive_enroll_plan(key_out: &str) -> Option<EnrollPlan> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    println!("No auth is configured (RELAY_TOKEN / RELAY_JWT_PUBLIC_KEY unset).");
    let answer = prompt("Enroll this relay with a kino-control instance? [y/N] ");
    if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
        return None;
    }
    let control_url = {
        let entered = prompt("kino-control URL [https://kino.samarthkombemane.com]: ");
        if entered.is_empty() {
            "https://kino.samarthkombemane.com".to_string()
        } else {
            entered
        }
    };
    let code = prompt("One-time enrollment code (from the kino-control web UI): ");
    let public_url = prompt("This relay's public URL (e.g. wss://relay.example.com): ");
    let name = prompt("Relay name (optional): ");
    if code.is_empty() || public_url.is_empty() {
        eprintln!("Enrollment needs an enrollment code and this relay's public URL - skipping.");
        return None;
    }
    Some(EnrollPlan {
        control_url,
        code,
        public_url,
        name: Some(name).filter(|n| !n.is_empty()),
        key_out: key_out.to_string(),
    })
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // ureq (enrollment) and axum-server TLS both sit on rustls 0.23, which
    // refuses to pick a crypto backend implicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // RELAY_JWT_PUBLIC_KEY is both "load the key from here" and, when
    // enrolling, "save the received key here" (default: control.pub.pem).
    let key_path = std::env::var("RELAY_JWT_PUBLIC_KEY")
        .ok()
        .filter(|p| !p.is_empty());
    let key_save_path = key_path.clone().unwrap_or_else(|| "control.pub.pem".to_string());

    // Env-driven self-enrollment (docker/systemd): all three set -> enroll on
    // startup, no questions asked.
    let env_plan = match (
        std::env::var("KINO_CONTROL_URL").ok().filter(|v| !v.is_empty()),
        std::env::var("KINO_ENROLL_CODE").ok().filter(|v| !v.is_empty()),
        std::env::var("RELAY_PUBLIC_URL").ok().filter(|v| !v.is_empty()),
    ) {
        (Some(control_url), Some(code), Some(public_url)) => Some(EnrollPlan {
            control_url,
            code,
            public_url,
            name: std::env::var("RELAY_NAME").ok().filter(|v| !v.is_empty()),
            key_out: key_save_path.clone(),
        }),
        _ => None,
    };

    let jwt_key = match &key_path {
        Some(path) if std::path::Path::new(path).exists() => {
            let pem = std::fs::read(path)
                .unwrap_or_else(|e| panic!("cannot read RELAY_JWT_PUBLIC_KEY ({path}): {e}"));
            Some(jsonwebtoken::DecodingKey::from_ed_pem(&pem).unwrap_or_else(|e| {
                panic!("RELAY_JWT_PUBLIC_KEY ({path}) is not an Ed25519 public key PEM: {e}")
            }))
        }
        // Set but missing is only fine when enrollment is about to create it.
        Some(path) if env_plan.is_none() => {
            panic!("RELAY_JWT_PUBLIC_KEY ({path}) does not exist")
        }
        _ => None,
    };

    let auth = AuthConfig {
        static_token: std::env::var("RELAY_TOKEN").ok().filter(|t| !t.is_empty()),
        jwt_key: std::sync::RwLock::new(jwt_key),
    };

    // A saved key means we're already enrolled - don't burn another code.
    let already_has_key = auth.jwt_key.read().unwrap().is_some();
    let enroll_plan = if already_has_key {
        if env_plan.is_some() {
            info!("Public key already present; skipping enrollment");
        }
        None
    } else {
        env_plan.or_else(|| {
            if auth.static_token.is_none() {
                interactive_enroll_plan(&key_save_path)
            } else {
                None
            }
        })
    };

    match (&auth.static_token, already_has_key, &enroll_plan) {
        (None, false, None) => warn!(
            "AUTH DISABLED - set RELAY_TOKEN and/or RELAY_JWT_PUBLIC_KEY, or enroll \
             with kino-control; anyone who can reach this relay can register agents \
             and open sessions"
        ),
        (st, jwt, plan) => info!(
            "Auth: static token: {}, kino-control JWT: {}",
            if st.is_some() { "yes" } else { "no" },
            if jwt {
                "yes"
            } else if plan.is_some() {
                "enrolling after startup"
            } else {
                "no"
            },
        ),
    }

    let state = AppState {
        agents: Arc::new(Mutex::new(HashMap::new())),
        pending_sessions: Arc::new(Mutex::new(HashMap::new())),
        auth: Arc::new(auth),
    };

    // Self-enrollment runs AFTER the server is listening: kino-control probes
    // our public /healthz before accepting, so enrolling earlier would always
    // fail. On success the key is persisted (restarts skip enrollment) and
    // installed into the live auth config - no restart needed.
    if let Some(plan) = enroll_plan {
        let auth = state.auth.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            for attempt in 1..=5u32 {
                let result = {
                    let plan_ref = &plan;
                    tokio::task::block_in_place(|| enroll_request(plan_ref))
                };
                match result {
                    Ok(pem) => {
                        match jsonwebtoken::DecodingKey::from_ed_pem(pem.as_bytes()) {
                            Ok(key) => {
                                if let Err(e) = std::fs::write(&plan.key_out, &pem) {
                                    warn!("Enrolled, but could not save key to {} ({e}) - re-enrollment will need a fresh code after a restart", plan.key_out);
                                } else {
                                    info!("Enrolled with {}; public key saved to {}", plan.control_url, plan.key_out);
                                }
                                *auth.jwt_key.write().expect("auth lock poisoned") = Some(key);
                                info!("kino-control token auth is now active");
                                return;
                            }
                            Err(e) => {
                                warn!("Enrollment returned an invalid key: {e}");
                                return;
                            }
                        }
                    }
                    Err(e) => warn!("Enrollment attempt {attempt}/5 failed: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            warn!("Enrollment failed after 5 attempts; relay continues without kino-control auth");
        });
    }

    let app = Router::new()
        .route("/healthz", get(health_handler))
        .route("/ws/control", get(ws_control_handler))
        .route("/ws/manager/request", get(ws_manager_handler))
        .route("/ws/agent/data", get(ws_agent_data_handler))
        .with_state(state);

    // PORT is what most hosts (Koyeb, Fly, Render) inject; BIND covers the rest.
    let bind = std::env::var("BIND").unwrap_or_else(|_| {
        let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
        format!("0.0.0.0:{port}")
    });
    let addr: SocketAddr = bind
        .parse()
        .unwrap_or_else(|e| panic!("BIND/PORT is not a valid socket address ({bind}): {e}"));

    // Terminate TLS ourselves only when handed a cert+key. Behind a TLS-terminating
    // proxy (nginx, Caddy) leave these unset and serve plaintext on the private side.
    match (std::env::var("TLS_CERT"), std::env::var("TLS_KEY")) {
        (Ok(cert), Ok(key)) => {
            // rustls 0.23 will not choose a crypto backend implicitly.
            let _ = rustls::crypto::ring::default_provider().install_default();

            let config = tls_config(&cert, &key)
                .unwrap_or_else(|e| panic!("failed to load TLS cert/key ({cert}, {key}): {e}"));

            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
            });

            info!("Relay listening on {} (TLS enabled - serving wss://)", addr);
            axum_server::bind_rustls(addr, config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
        _ => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
            info!(
                "Relay listening on {} (plaintext - terminate TLS at your proxy)",
                addr
            );
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .unwrap();
        }
    }
}

/// Liveness probe for Docker/systemd/platform health checks.
async fn health_handler() -> impl IntoResponse {
    "ok"
}

/// Build the TLS config from a PEM cert chain + private key.
///
/// We pin ALPN to http/1.1 deliberately. axum-server's default advertises h2 as
/// well, and a client that takes it can never perform a WebSocket upgrade - that
/// handshake is an HTTP/1.1 mechanism, so the connection just 400s. This relay
/// serves nothing but WebSockets, so h2 has no upside here.
fn tls_config(cert_path: &str, key_path: &str) -> Result<RustlsConfig, String> {
    let cert_pem = std::fs::read(cert_path).map_err(|e| format!("cannot read cert: {e}"))?;
    let key_pem = std::fs::read(key_path).map_err(|e| format!("cannot read key: {e}"))?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid cert PEM: {e}"))?;
    if certs.is_empty() {
        return Err("no certificates found in cert file".to_string());
    }

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| format!("invalid key PEM: {e}"))?
        .ok_or_else(|| "no private key found in key file".to_string())?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("cert/key mismatch: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(RustlsConfig::from_config(Arc::new(config)))
}

/// Resolve when the process is asked to stop, so in-flight sessions aren't cut
/// mid-frame by an abrupt kill. Docker and systemd both send SIGTERM.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot listen for SIGTERM: {e}");
            return;
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT received, shutting down"),
        _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
    }
}

async fn ws_control_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<ControlQuery>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    authorize(&state.auth, &headers, Role::Agent, Some(&query.agent_id)).inspect_err(|_| {
        warn!("Rejected unauthorized control connection for agent id {}", query.agent_id)
    })?;
    Ok(ws.on_upgrade(move |socket| handle_control_socket(socket, query.agent_id, state)))
}

async fn handle_control_socket(socket: WebSocket, agent_id: String, state: AppState) {
    let (tx, mut rx) = mpsc::channel::<ControlMessage>(32);
    let token = Uuid::new_v4();

    {
        let mut agents = state.agents.lock().await;
        info!("Agent connected: {}", agent_id);
        agents.insert(agent_id.clone(), AgentHandle { token, tx });
    }

    let (mut ws_tx, mut ws_rx) = socket.split();

    let send_task = tokio::spawn(async move {
        // Send periodic pings so idle NAT/load-balancer timeouts don't silently
        // reap the control channel, and so a half-open connection eventually
        // surfaces as a write error rather than hanging forever.
        let mut ping_interval = tokio::time::interval(Duration::from_secs(30));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                maybe_msg = rx.recv() => {
                    match maybe_msg {
                        Some(msg) => {
                            let json = serde_json::to_string(&msg).unwrap();
                            if ws_tx.send(Message::Text(json)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        // Drain the socket (pongs, etc.). Returns when the peer closes or errors,
        // which is our signal that the agent has gone away.
        while let Some(Ok(_msg)) = ws_rx.next().await {
            // just consume
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    info!("Agent disconnected: {}", agent_id);
    // Only evict if we still own the slot; a reconnect may have replaced us.
    let mut agents = state.agents.lock().await;
    if agents.get(&agent_id).map(|h| h.token) == Some(token) {
        agents.remove(&agent_id);
    }
}

async fn ws_manager_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<ManagerQuery>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    authorize(&state.auth, &headers, Role::Manager, Some(&query.agent_id)).inspect_err(|_| {
        warn!("Rejected unauthorized manager request for agent id {}", query.agent_id)
    })?;
    Ok(ws.on_upgrade(move |socket| handle_manager_socket(socket, query.agent_id, state)))
}

async fn handle_manager_socket(manager_socket: WebSocket, agent_id: String, state: AppState) {
    let session_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<WebSocket>();

    // Register the pending session *before* notifying the agent. Otherwise a fast
    // agent could open its data socket before we've inserted the slot, and the
    // data socket would be dropped as "unknown session".
    state.pending_sessions.lock().await.insert(session_id.clone(), tx);

    // Grab the sender and drop the map lock before awaiting the send, so a full
    // control-channel buffer can't stall the whole agent registry.
    let agent_tx = {
        let agents = state.agents.lock().await;
        agents.get(&agent_id).map(|h| h.tx.clone())
    };
    match agent_tx {
        Some(agent_tx) => {
            let msg = ControlMessage::NewConnection { session_id: session_id.clone() };
            if agent_tx.send(msg).await.is_err() {
                warn!("Failed to send connection request to agent {}", agent_id);
                state.pending_sessions.lock().await.remove(&session_id);
                return;
            }
        }
        None => {
            warn!("Manager requested connection for offline agent {}", agent_id);
            state.pending_sessions.lock().await.remove(&session_id);
            return;
        }
    }

    let agent_socket = match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(socket)) => socket,
        _ => {
            warn!("Agent {} failed to open data socket in time", agent_id);
            state.pending_sessions.lock().await.remove(&session_id);
            return;
        }
    };

    info!("Bridging session {} for agent {}", session_id, agent_id);
    bridge_sockets(manager_socket, agent_socket).await;
    info!("Session {} closed", session_id);
}

async fn ws_agent_data_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<AgentDataQuery>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    authorize(&state.auth, &headers, Role::Agent, None).inspect_err(|_| {
        warn!("Rejected unauthorized data socket for session {}", query.session_id)
    })?;
    Ok(ws.on_upgrade(move |socket| handle_agent_data_socket(socket, query.session_id, state)))
}

async fn handle_agent_data_socket(socket: WebSocket, session_id: String, state: AppState) {
    let tx = state.pending_sessions.lock().await.remove(&session_id);
    if let Some(tx) = tx {
        let _ = tx.send(socket);
    } else {
        warn!("Agent data socket connected for unknown/expired session: {}", session_id);
    }
}

async fn bridge_sockets(manager: WebSocket, agent: WebSocket) {
    let (mut m_tx, mut m_rx) = manager.split();
    let (mut a_tx, mut a_rx) = agent.split();

    let m_to_a = tokio::spawn(async move {
        while let Some(msg) = m_rx.next().await {
            match msg {
                Ok(m) => {
                    if a_tx.send(m).await.is_err() { break; }
                }
                Err(_) => break,
            }
        }
    });

    let a_to_m = tokio::spawn(async move {
        while let Some(msg) = a_rx.next().await {
            match msg {
                Ok(m) => {
                    if m_tx.send(m).await.is_err() { break; }
                }
                Err(_) => break,
            }
        }
    });

    tokio::select! {
        _ = m_to_a => {},
        _ = a_to_m => {},
    }
}
