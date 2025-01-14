use std::collections::HashMap;
use std::future::Future;
use crate::model::file::FileDto;
use crate::webrtc::signaling::{ManagedSignalingConnection, WsServerSdpMessage};
use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::engine::GeneralPurpose;
use base64::Engine;
use bytes::{Bytes, BytesMut};
use std::io::{Read, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;
use uuid::Uuid;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::{math_rand_alpha, RTCPeerConnection};

#[derive(Debug, Deserialize, Serialize)]
struct RTCInitialMessage {
    pub files: Vec<FileDto>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RTCInitialResponse {
    pub files: HashMap<String, String>,
}

pub struct RTCSendMessage {
    pub file_id: String,
    pub binary: mpsc::Receiver<Vec<u8>>,
}

pub enum RTCReceiveMessage {
    Text(String),
    Binary(mpsc::Sender<Vec<u8>>),
}

pub enum RTCStatus {
    /// Received remote SDP offer/answer. Ready to start P2P connection.
    SdpExchanged,

    /// Opened data channel. Ready to send/receive data.
    Connected,

    /// Data channel closed. Connection is closed.
    Finished,

    /// Error occurred. Connection is closed.
    Error(String),
}

pub struct FileError {
    pub file_id: String,
    pub error: String,
}

pub async fn send_offer(
    signaling: &ManagedSignalingConnection,
    target_id: Uuid,
    files: Vec<FileDto>,
    status_tx: mpsc::Sender<RTCStatus>,
    error_tx: mpsc::Sender<FileError>,
    receiving_tx: mpsc::Sender<String>,
    mut sending_rx: mpsc::Receiver<RTCSendMessage>,
) -> Result<()> {
    let peer_connection = create_peer_connection().await?;

    let data_channel = peer_connection
        .create_data_channel(
            "data",
            Some(RTCDataChannelInit {
                ordered: Some(true),
                max_packet_life_time: None,
                max_retransmits: None,
                protocol: None,
                negotiated: None,
            }),
        )
        .await?;


    {
        let data_channel_clone = Arc::clone(&data_channel);
        data_channel.on_open(Box::new(move || {
            let data_channel = Arc::clone(&data_channel_clone);
            Box::pin(async move {
                status_tx
                    .try_send(RTCStatus::Connected)
                    .expect("Failed to send status");

                // send initial message
                let initial_message = RTCInitialMessage {
                    files,
                };
                let initial_message_str = serde_json::to_string(&initial_message).unwrap();
                let result = data_channel.send(&Bytes::from(initial_message_str)).await;
                if let Err(e) = result {
                    error_tx
                        .try_send(FileError {
                            file_id: "".to_string(),
                            error: e.to_string(),
                        })
                        .expect("Failed to send error");

                    status_tx
                        .try_send(RTCStatus::Error(e.to_string()))
                        .expect("Failed to send status");

                    return;
                }

                while let Some(mut message) = sending_rx.recv().await {
                    let result = data_channel.send_text(message.file_id.clone()).await.map_err(|err| format!("{err}"));
                    match result {
                        Ok(_) => {
                            let result = process_in_chunks(Arc::clone(&data_channel), message.binary,  |data_channel, chunk| {
                                Box::pin({
                                    let data_channel = Arc::clone(&data_channel);
                                    async move {
                                        let result = data_channel.send(&chunk).await.map_err(|err| format!("{err}"));
                                        match result {
                                            Ok(_) => Ok(()),
                                            Err(e) => Err(anyhow!("{e}")),
                                        }
                                    }
                                })
                            }).await;

                            if let Err(e) = result {
                                error_tx
                                    .try_send(FileError {
                                        file_id: message.file_id,
                                        error: e.to_string(),
                                    })
                                    .expect("Failed to send error");
                            }
                        }
                        Err(e) => {
                            error_tx
                                .try_send(FileError {
                                    file_id: message.file_id,
                                    error: e,
                                })
                                .expect("Failed to send error");
                        }
                    }
                }
            })
        }));
    }

    // Register text message handling
    data_channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
        Box::pin({
            let tx = receiving_tx.clone();
            async move {
                tx.send(msg_str).await.expect("Failed to send received message");
            }
        })
    }));

    let offer = peer_connection.create_offer(None).await?;
    let mut gather_complete = peer_connection.gathering_complete_promise().await;
    peer_connection.set_local_description(offer).await?;
    let _ = gather_complete.recv().await;

    let session_id = Uuid::new_v4().to_string();
    let local_description = peer_connection
        .local_description()
        .await
        .ok_or_else(|| anyhow::anyhow!("generate local_description failed!"))?;

    signaling
        .send_offer(
            session_id.clone(),
            target_id,
            encode_sdp(&local_description.sdp),
        )
        .await?;

    let (tx_answer, rx_answer) = tokio::sync::oneshot::channel();

    signaling
        .on_answer(session_id, |message| {
            tx_answer.send(message.sdp).unwrap();
        })
        .await;

    let remote_desc = rx_answer.await?;
    let answer = RTCSessionDescription::answer(decode_sdp(&remote_desc)?)?;

    peer_connection.set_remote_description(answer).await?;

    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        if s == RTCPeerConnectionState::Failed {
            println!("Peer Connection has gone to failed exiting");
            let _ = done_tx.try_send(());
        }
        Box::pin(async {})
    }));

    done_rx.recv().await;

    peer_connection.close().await?;

    Ok(())
}

pub async fn accept_offer(
    signaling: &ManagedSignalingConnection,
    offer: &WsServerSdpMessage,
) -> Result<()> {
    let peer_connection = create_peer_connection().await?;

    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        println!("Peer Connection State has changed: {s}");

        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            println!("Peer Connection has gone to failed exiting");
            let _ = done_tx.try_send(());
        }

        Box::pin(async {})
    }));

    let close_after = Arc::new(AtomicI32::new(32));

    // Register data channel creation handling
    peer_connection
        .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
            let d_label = d.label().to_owned();
            let d_id = d.id();
            println!("New DataChannel {d_label} {d_id}");

            let close_after2 = Arc::clone(&close_after);

            // Register channel opening handling
            Box::pin(async move {
                let d2 = Arc::clone(&d);
                let d_label2 = d_label.clone();
                let d_id2 = d_id;
                d.on_open(Box::new(move || {
                    println!("Data channel '{d_label2}'-'{d_id2}' open. Random messages will now be sent to any connected DataChannels every 5 seconds");
                    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
                    let done_tx = Arc::new(Mutex::new(Some(done_tx)));
                    Box::pin(async move {
                        d2.on_close(Box::new(move || {
                            println!("Data channel '{d_label2}'-'{d_id2}' closed.");
                            let done_tx2 = Arc::clone(&done_tx);
                            Box::pin(async move{
                                let mut done = done_tx2.lock().await;
                                done.take();
                            })
                        }));

                        let mut result = Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(Duration::from_secs(5));
                            tokio::pin!(timeout);

                            tokio::select! {
                                _ = done_rx.recv() => {
                                    break;
                                }
                                _ = timeout.as_mut() =>{
                                    let message = math_rand_alpha(15);
                                    println!("Sending '{message}'");
                                    result = d2.send_text(message).await.map_err(Into::into);

                                    let cnt = close_after2.fetch_sub(1, Ordering::SeqCst);
                                    if cnt <= 0 {
                                        println!("Sent times out. Closing data channel '{}'-'{}'.", d2.label(), d2.id());
                                        let _ = d2.close().await;
                                        break;
                                    }
                                }
                            };
                        }
                    })
                }));

                // Register text message handling
                d.on_message(Box::new(move |msg: DataChannelMessage| {
                    let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                    println!("Message from DataChannel '{d_label}': '{msg_str}'");
                    Box::pin(async {})
                }));
            })
        }));

    let remote_desc_sdp = decode_sdp(&offer.sdp)?;
    let remote_desc = RTCSessionDescription::offer(remote_desc_sdp)?;
    peer_connection.set_remote_description(remote_desc).await?;

    let answer = peer_connection.create_answer(None).await?;

    let mut gather_complete = peer_connection.gathering_complete_promise().await;
    peer_connection.set_local_description(answer).await?;
    let _ = gather_complete.recv().await;

    let local_description = peer_connection
        .local_description()
        .await
        .ok_or_else(|| anyhow::anyhow!("generate local_description failed!"))?;

    signaling
        .send_answer(
            offer.session_id.clone(),
            offer.peer.id,
            encode_sdp(&local_description.sdp),
        )
        .await?;

    done_rx.recv().await;

    peer_connection.close().await?;

    Ok(())
}

async fn create_peer_connection() -> Result<Arc<RTCPeerConnection>> {
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    let peer_connection = api.new_peer_connection(config).await?;

    Ok(Arc::new(peer_connection))
}

const BASE_64_SDP: GeneralPurpose = URL_SAFE_NO_PAD;

fn encode_sdp(s: &str) -> String {
    let mut compressor = brotli::CompressorWriter::new(Vec::new(), 4096, 11, 24);
    compressor.write_all(s.as_bytes()).unwrap();
    BASE_64_SDP.encode(&compressor.into_inner())
}

fn decode_sdp(s: &str) -> Result<String> {
    let decoded_data = BASE_64_SDP.decode(s)?;
    let mut decompressor = brotli::Decompressor::new(&decoded_data[..], 4096);
    let mut decompressed = Vec::new();
    decompressor
        .read_to_end(&mut decompressed)
        .expect("Decompression failed");
    let result = String::from_utf8(decompressed)?;
    Ok(result)
}

const CHUNK_SIZE: usize = 16 * 1024; // 16 KiB

pub async fn process_in_chunks<T, F>(
    data_channel: T,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut callback: F,
) -> Result<()>
where
    F: FnMut(&T, Bytes) -> Pin<Box<dyn Future<Output=Result<()>> + Send>> + Send,
{
    let mut buffer = BytesMut::with_capacity(CHUNK_SIZE);

    while let Some(data) = rx.recv().await {
        // Append new data to the buffer
        buffer.extend_from_slice(&data);

        // While the buffer has enough data, split off CHUNK_SIZE
        while buffer.len() >= CHUNK_SIZE {
            let chunk = buffer.split_to(CHUNK_SIZE).freeze();
            callback(&data_channel, chunk).await?;
        }
    }

    // After the channel is closed, if there's leftover data, handle it as needed:
    if !buffer.is_empty() {
        callback(&data_channel, buffer.freeze()).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_in_chunks() {
        let (tx, rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let mut test_vec = vec![0; CHUNK_SIZE * 2 + 5];
            test_vec[CHUNK_SIZE..CHUNK_SIZE * 2].iter_mut().for_each(|x| *x = 1);
            test_vec[CHUNK_SIZE * 2..].iter_mut().for_each(|x| *x = 2);
            tx.send(test_vec).await.unwrap();
        });

        let mut chunks = Vec::new();

        let result = process_in_chunks(&0, rx, |_, chunk| {
            chunks.push(chunk);
            Box::pin(async { Ok(()) })
        })
        .await;

        assert!(result.is_ok());
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), CHUNK_SIZE);
        assert_eq!(chunks[1].len(), CHUNK_SIZE);
        assert_eq!(chunks[2].len(), 5);

        assert_eq!(chunks[0].iter().all(|x| *x == 0), true);
        assert_eq!(chunks[1].iter().all(|x| *x == 1), true);
        assert_eq!(chunks[2].iter().all(|x| *x == 2), true);
    }
}
