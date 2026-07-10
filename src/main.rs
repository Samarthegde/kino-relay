use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, oneshot};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    pending_sessions: Arc<Mutex<HashMap<String, oneshot::Sender<WebSocket>>>>,
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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = AppState {
        agents: Arc::new(Mutex::new(HashMap::new())),
        pending_sessions: Arc::new(Mutex::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/ws/control", get(ws_control_handler))
        .route("/ws/manager/request", get(ws_manager_handler))
        .route("/ws/agent/data", get(ws_agent_data_handler))
        .with_state(state);

    let addr = "0.0.0.0:3000";
    info!("Relay server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_control_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<ControlQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_control_socket(socket, query.agent_id, state))
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
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_manager_socket(socket, query.agent_id, state))
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
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_agent_data_socket(socket, query.session_id, state))
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
