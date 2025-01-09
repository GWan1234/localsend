use crate::config::error::AppError;
use crate::config::state::{AppState, IpRequestCountMap, TxMap};
use crate::util;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::Json;
use localsend::webrtc::signaling::{SignalingRequestData, WsMessageType, WsServerMessage};
use std::net::SocketAddr;
use std::sync::LazyLock;
use tokio::sync::mpsc;
use uuid::Uuid;

static MAX_REQUEST: LazyLock<u32> = LazyLock::new(|| {
    std::env::var("MAX_REQUEST_PER_IP_PER_DAY")
        .unwrap_or_else(|_| "1000".to_string())
        .parse::<u32>()
        .unwrap()
});

pub async fn send_offer(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<SignalingRequestData>,
) -> Result<(), AppError> {
    let ip_group = util::ip::get_ip_group(addr.ip());

    protect_ddos(state.request_count_map, &ip_group).await?;

    send_to_peer_with_lock(
        ip_group,
        payload.target,
        &WsServerMessage {
            ws_type: WsMessageType::Offer,
            members: None,
            peer: Some(payload.info),
            peer_id: None,
            sdp: Some(payload.sdp),
        },
        &state.tx_map,
    )
    .await;

    Ok(())
}

pub async fn send_answer(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<SignalingRequestData>,
) -> Result<(), AppError> {
    let ip_group = util::ip::get_ip_group(addr.ip());

    protect_ddos(state.request_count_map, &ip_group).await?;

    send_to_peer_with_lock(
        ip_group,
        payload.target,
        &WsServerMessage {
            ws_type: WsMessageType::Answer,
            members: None,
            peer: Some(payload.info),
            peer_id: None,
            sdp: Some(payload.sdp),
        },
        &state.tx_map,
    )
    .await;

    Ok(())
}

async fn protect_ddos(
    request_count_map: IpRequestCountMap,
    ip_group: &str,
) -> Result<(), AppError> {
    let mut request_count_map = request_count_map.lock().await;
    let count = request_count_map.entry(ip_group.to_string()).or_insert(0);
    if *count >= *MAX_REQUEST {
        return Err(AppError::status(StatusCode::TOO_MANY_REQUESTS, None));
    }
    *count += 1;
    Ok(())
}

async fn send_to_peer_with_lock(
    ip_group: String,
    peer_id: Uuid,
    message: &WsServerMessage,
    tx_map: &TxMap,
) {
    let mut tx: Option<mpsc::Sender<WsServerMessage>> = None;
    {
        let tx_map = tx_map.lock().await;
        if let Some(tx_local_map) = tx_map.get(&ip_group) {
            if let Some(peer_state) = tx_local_map.get(&peer_id) {
                tx = Some(peer_state.tx.clone());
            }
        }
    }

    if let Some(tx) = tx {
        let _ = tx.send(message.clone()).await;
    }
}
