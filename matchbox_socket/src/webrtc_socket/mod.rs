//! The webrtc_socket module provides a unified API for dealing with WebRtc on both native and wasm
//! builds. This common API only implements behaviors needed by the rest of matchbox_socket: it is
//! not a general WebRtc abstraction.

pub mod error;
mod messages;
mod signal_peer;
mod socket;

use self::error::SignalingError;
use crate::{Error, webrtc_socket::signal_peer::SignalPeer};
use async_trait::async_trait;
use cfg_if::cfg_if;
use futures::{
    Future, FutureExt, StreamExt,
    future::{Either, Fuse},
    stream::FuturesUnordered,
};
use futures_channel::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot::{self, Canceled},
};
use futures_timer::Delay;
use futures_util::select;
use log::{debug, error, warn};
use matchbox_protocol::PeerId;
pub use messages::*;
pub(crate) use socket::MessageLoopChannels;
use socket::SocketConfig;
pub use socket::{
    ChannelConfig, PeerState, RtcIceServerConfig, WebRtcChannel, WebRtcSocket, WebRtcSocketBuilder,
};
use std::{collections::HashMap, mem::take, pin::Pin, sync::Arc};
use webrtc::turn::proto::channum::ChannelNumber;

cfg_if! {
    if #[cfg(target_arch = "wasm32")] {
        mod wasm;
        type UseMessenger = wasm::WasmMessenger;
        type UseSignallerBuilder = wasm::WasmSignallerBuilder;
        /// A future which runs the message loop for the socket and completes
        /// when the socket closes or disconnects
        pub type MessageLoopFuture = Pin<Box<dyn Future<Output = Result<(), Error>>>>;
    } else {
        mod native;
        type UseMessenger = native::NativeMessenger;
        type UseSignallerBuilder = native::NativeSignallerBuilder;
        /// A future which runs the message loop for the socket and completes
        /// when the socket closes or disconnects
        pub type MessageLoopFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;
    }
}

/// A builder that constructs a new [Signaller] from a room URL.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait SignallerBuilder: std::fmt::Debug + Sync + Send + 'static {
    /// Create a new [Signaller]. The Room URL is an implementation specific identifier for joining
    /// a room.
    async fn new_signaller(
        &self,
        attempts: Option<u16>,
        room_url: String,
    ) -> Result<Box<dyn Signaller>, SignalingError>;
}

/// A signalling implementation.
///
/// The Signaller is responsible for passing around
/// [WebRTC signals](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Signaling_and_video_calling#the_signaling_server)
/// between room peers, encoded as [PeerEvent::Signal] which holds a [PeerSignal].
///
/// It is also responsible for notifying each peer of the following special events:
///
/// - [PeerEvent::IdAssigned] -- first event in stream, received once at connection time.
/// - [PeerEvent::NewPeer] -- received when a new peer has joined the room, AND when this node must
///   send an offer to them.
/// - [PeerEvent::PeerLeft] -- received when a peer leaves the room.
///
/// To achieve a full mesh configuration, the signaller must do the following for **each pair** of
/// peers in a room:
///
/// 1. It decides which peer is the "Offerer" and which is the "Answerer".
/// 2. It sends [PeerEvent::NewPeer] to the "Offerer" only.
/// 3. It passes [PeerEvent::Signal] events back and forth between them, until one of them
///    disconnects.
/// 4. It sends [PeerEvent::PeerLeft] with the ID of the disconnected peer to the remaining peer.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Signaller: Sync + Send + 'static {
    /// Request the signaller to pass a message to another peer.
    async fn send(&mut self, request: PeerRequest) -> Result<(), SignalingError>;

    /// Get the next event from the signaller.
    async fn next_message(&mut self) -> Result<PeerEvent, SignalingError>;
}

async fn signaling_loop(
    builder: Arc<dyn SignallerBuilder>,
    attempts: Option<u16>,
    room_url: String,
    mut requests_receiver: futures_channel::mpsc::UnboundedReceiver<PeerRequest>,
    events_sender: futures_channel::mpsc::UnboundedSender<PeerEvent>,
) -> Result<(), SignalingError> {
    let mut signaller = builder.new_signaller(attempts, room_url).await?;

    loop {
        select! {
            request = requests_receiver.next().fuse() => {
                debug!("-> {request:?}");
                let Some(request) = request else {break Err(SignalingError::StreamExhausted)};
                signaller.send(request).await?;
            }

            message = signaller.next_message().fuse() => {
                match message {
                    Ok(message) => {
                        debug!("Received {message:?}");
                        events_sender.unbounded_send(message).map_err(SignalingError::from)?;
                    }
                    Err(SignalingError::UnknownFormat) => {
                        warn!("ignoring unexpected non-text message from signaling server")
                    },
                    Err(err) => break Err(err)
                }

            }

            complete => break Ok(())
        }
    }

    // TODO: should this `events_sender.close_channel();` to communicate disconnects?
}

/// The raw format of data being sent and received.
pub type Packet = Box<[u8]>;

/// Errors that can happen when sending packets
#[derive(Debug, thiserror::Error)]
#[error("The socket was dropped and package could not be sent")]
pub struct PacketSendError {
    #[cfg(not(target_arch = "wasm32"))]
    source: futures_channel::mpsc::SendError,
    #[cfg(target_arch = "wasm32")]
    source: error::JsError,
}

pub(crate) trait PeerDataSender {
    fn send(&mut self, packet: Packet) -> Result<(), PacketSendError>;
}

struct HandshakeResult<M> {
    peer_id: PeerId,
    metadata: M,
}

/// A platform independent abstraction for the subset of Web Rtc APIs needed by matchbox_socket.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
trait Messenger<Tx: DataChannelEventReceiver> {
    type DataChannel: PeerDataSender; // TODO: MatchboxDataChannel
    type HandshakeMeta: Send;

    async fn offer_handshake(
        signal_peer: SignalPeer,
        mut peer_signal_rx: UnboundedReceiver<PeerSignal>,
        builders: Vec<ChannelBuilder<Tx>>,
        ice_server_config: &RtcIceServerConfig,
        data_channels_ready_fut: Pin<
            Box<Fuse<impl Future<Output = Result<(), Canceled>> + std::marker::Send>>,
        >,
    ) -> HandshakeResult<Self::HandshakeMeta>;

    async fn accept_handshake(
        signal_peer: SignalPeer,
        peer_signal_rx: UnboundedReceiver<PeerSignal>,
        builders: Vec<ChannelBuilder<Tx>>,
        ice_server_config: &RtcIceServerConfig,
        data_channels_ready_fut: Pin<
            Box<Fuse<impl Future<Output = Result<(), Canceled>> + std::marker::Send>>,
        >,
    ) -> HandshakeResult<Self::HandshakeMeta>;
}

struct ChannelBuilder<Tx: DataChannelEventReceiver> {
    /// Where to send all incoming events triggered by the channel.
    event_handlers: Tx,
    /// Configuration for the channel to build.
    channel_config: ChannelConfig,
}

/// Receiver for events from a [MatchboxDataChannel].
///
/// Receives events from a [RTCDataChannel](https://developer.mozilla.org/en-US/docs/Web/API/RTCDataChannel).
trait DataChannelEventReceiver: Send + Sync + 'static {
    fn on_open(&mut self);
    fn on_close(&mut self);
    fn on_error(&mut self, message: String);
    fn on_message(&mut self, packet: Packet);
    /// [bufferedamountlow event](https://developer.mozilla.org/en-US/docs/Web/API/RTCDataChannel/bufferedamountlow_event).
    fn on_buffered_amount_low(&mut self);
}

/// Message from a specific Peer.
/// Aggregates messages from all channels to that Peer.
enum PeerMessage {
    /// The Peer has connected, and finished opening all requested channels.
    Join,

    /// A packet has arrived on a channel between the Join and Close.
    /// Packets from during the Join are buffered until after the Join.
    /// Packets from after the Close are discarded.
    Packet(ChannelNumber, Packet),

    /// Connection to this peer is no longer available.
    ///
    /// Triggered by the first channel to close or error after a `Join`.
    /// If a string is provided, it is the error message. If none is provided, an orderly close
    /// occured (for at least the first channel to close or error).
    Close(Option<String>),
}

async fn message_loop<M: Messenger<AsyncDataChannelReceiver>>(
    id_tx: futures_channel::oneshot::Sender<PeerId>,
    config: SocketConfig,
    channels: MessageLoopChannels,
) -> Result<(), SignalingError> {
    let MessageLoopChannels {
        requests_sender,
        mut events_receiver,
        mut peer_messages_out_rx,
        messages_from_peers_tx,
        peer_state_tx,
    } = channels;

    // TODO: Remove need to forward from messages_from_peers to messages_from_peers_tx
    let (messages_from_peers_tx_2, mut messages_from_peers) =
        futures_channel::mpsc::unbounded::<(PeerId, PeerMessage)>();

    let handshakes = FuturesUnordered::new();
    let mut handshake_signals: HashMap<PeerId, futures_channel::mpsc::UnboundedSender<PeerSignal>> =
        HashMap::new();

    // TODO: might need to send errors when join fails.
    let mut ready_signals: HashMap<PeerId, oneshot::Sender<()>> = HashMap::new();

    let mut id_tx = Option::Some(id_tx);

    let mut timeout = if let Some(interval) = config.keep_alive_interval {
        Either::Left(Delay::new(interval))
    } else {
        Either::Right(std::future::pending())
    }
    .fuse();

    let setup_peer =
        |sender: PeerId,
         handshake_signals: &mut HashMap<PeerId, _>,
         ready_signals: &mut HashMap<PeerId, oneshot::Sender<()>>| {
            let (signal_tx, signal_rx) = futures_channel::mpsc::unbounded();
            let (ready_sender, ready_receiver) = oneshot::channel();
            handshake_signals.insert(sender, signal_tx.clone());
            ready_signals.insert(sender, ready_sender);
            let signal_peer = SignalPeer::new(sender, requests_sender.clone());
            let builders = AsyncDataChannelReceiver::channels_for_peer(
                sender,
                &config.channels,
                messages_from_peers_tx_2.clone(),
            );
            (
                signal_peer,
                signal_rx,
                builders,
                Box::pin(ready_receiver.fuse()),
                signal_tx,
            )
        };

    loop {
        select! {
            _  = &mut timeout => {
                if requests_sender.unbounded_send(PeerRequest::KeepAlive).is_err() {
                    // socket dropped
                    break Ok(());
                }
                if let Some(interval) = config.keep_alive_interval {
                    timeout = Either::Left(Delay::new(interval)).fuse();
                } else {
                    error!("no keep alive timeout, please file a bug");
                }
            }

            message = events_receiver.next().fuse() => {
                if let Some(event) = message {
                    debug!("{event:?}");
                    match event {
                        PeerEvent::IdAssigned(peer_uuid) => {
                            if id_tx.take().expect("already sent peer id").send(peer_uuid.to_owned()).is_err() {
                                // Socket receiver was dropped, exit cleanly.
                                break Ok(());
                            };
                        },
                        PeerEvent::NewPeer(sender) => {
                            let (signal_peer, signal_rx, builders, ready_receiver, _from_peer_tx) = setup_peer(sender, &mut handshake_signals, &mut ready_signals);
                            handshakes.push(M::offer_handshake(signal_peer, signal_rx, builders, &config.ice_server, ready_receiver))
                        },
                        PeerEvent::PeerLeft(peer_uuid) => {
                            // TODO: Better cleanup
                            handshake_signals.remove(&peer_uuid);
                        },
                        PeerEvent::Signal { sender, data } => {
                            let (signal_peer, signal_rx, builders, ready_receiver, from_peer_tx) = setup_peer(sender, &mut handshake_signals, &mut ready_signals);
                            let signal_tx = handshake_signals.entry(sender).or_insert_with(|| {
                                handshakes.push(M::accept_handshake(signal_peer, signal_rx, builders, &config.ice_server, ready_receiver), );
                                from_peer_tx
                            });

                            if signal_tx.unbounded_send(data).is_err() {
                                warn!("ignoring signal from peer {sender} because the handshake has already finished");
                            }
                        },
                    }
                }
            }

            message = messages_from_peers.next().fuse() => {
                match message {
                    Some((peer,PeerMessage::Join))=>{
                            ready_signals.remove(&peer).unwrap().send(());
                            peer_state_tx.unbounded_send((peer, PeerState::Connected));
                        }
                        Some((peer,PeerMessage::Packet(channel, packet)))=>{
                            messages_from_peers_tx[channel.0 as usize].unbounded_send((peer, packet));
                        }
                        Some((peer,PeerMessage::Close(error)))=>{
                            peer_state_tx.unbounded_send((peer, PeerState::Disconnected));
                        }
                    None => {
                        // Receiver end of outgoing message channel closed,
                        // which most likely means the socket was dropped.
                        // There could probably be cleaner ways to handle this,
                        // but for now, just exit cleanly.
                        warn!("Outgoing message queue closed, message not sent");
                        break Ok(());
                    }
                }
            }

            complete => break Ok(())
        }
    }
}

pub struct AsyncDataChannelReceiver {
    /// Shared reference to the UnboundedPeerReceiver for this peer.
    /// All channels for the peer share this reference, and use it to coordinate.
    peer: Arc<std::sync::Mutex<UnboundedPeerReceiver>>,
    /// Which channel this is for.
    channel: ChannelNumber,
}

/// Forwards messages from all channels for a given peer
pub struct UnboundedPeerReceiver {
    remote_id: PeerId,
    /// Starts a number of channels and counts down as they connect.
    /// Connection event occurs when this hits zero meaning all channels are connected.
    connecting_count: usize,
    closed: bool,

    /// Buffer of packets which name in before connecting_count reached 0.
    buffer: Option<Vec<(ChannelNumber, Packet)>>,

    messages_from_peers_tx: UnboundedSender<(PeerId, PeerMessage)>,
}

impl UnboundedPeerReceiver {
    fn channel_open(&mut self) {
        if self.closed {
            return;
        }

        // Decrement number of connecting_count
        self.connecting_count -= 1;

        if self.connecting_count == 0 {
            // If this is the last channel to connect based on connecting_count, send Join message.
            self.forward(PeerMessage::Join);

            // Flush buffer
            let buffer = take(&mut self.buffer).unwrap();
            for b in buffer.into_iter() {
                self.forward(PeerMessage::Packet(b.0, b.1));
            }
        }
    }

    fn channel_close(&mut self, error: Option<String>) {
        if self.closed {
            return;
        }
        // If joined was sent, send close event.
        if self.connecting_count == 0 {
            self.forward(PeerMessage::Close(error));
        }
        self.closed = true;
        // Avoid holding onto unneeded memory.
        self.buffer = None;
    }

    fn forward(&self, msg: PeerMessage) {
        assert!(!self.closed);
        self.messages_from_peers_tx
            .unbounded_send((self.remote_id, msg))
            .unwrap();
    }

    fn channel_packet(&mut self, channel: ChannelNumber, msg: Packet) {
        if let Some(buffer) = &mut self.buffer {
            buffer.push((channel, msg))
        } else {
            if !self.closed {
                self.forward(PeerMessage::Packet(channel, msg));
            }
        }
    }
}

impl AsyncDataChannelReceiver {
    pub fn channels_for_peer(
        remote_id: PeerId,
        config: &[ChannelConfig],
        messages_from_peers_tx: UnboundedSender<(PeerId, PeerMessage)>,
    ) -> Vec<ChannelBuilder<AsyncDataChannelReceiver>> {
        assert!(config.len() > 0);

        let peer = UnboundedPeerReceiver {
            remote_id,
            connecting_count: config.len(),
            closed: false,
            buffer: Some(vec![]),
            messages_from_peers_tx,
        };

        let peer: Arc<std::sync::Mutex<UnboundedPeerReceiver>> = Arc::new(peer.into());

        config
            .iter()
            .enumerate()
            .map(|(channel, channel_config)| ChannelBuilder {
                channel_config: channel_config.clone(),

                event_handlers: AsyncDataChannelReceiver {
                    // Number channel based off of index.
                    // Assumes number of channels fits in a u16.
                    channel: ChannelNumber(channel.try_into().unwrap()),
                    peer: peer.clone(),
                },
            })
            .collect()
    }
}

impl DataChannelEventReceiver for AsyncDataChannelReceiver {
    fn on_open(&mut self) {
        self.peer.lock().unwrap().channel_open();
    }

    fn on_close(&mut self) {
        self.peer.lock().unwrap().channel_close(None);
    }

    fn on_error(&mut self, message: String) {
        warn!("data channel error {message}");
        self.peer.lock().unwrap().channel_close(Some(message));
    }

    fn on_message(&mut self, packet: Packet) {
        self.peer
            .lock()
            .unwrap()
            .channel_packet(self.channel, packet);
    }

    fn on_buffered_amount_low(&mut self) {
        todo!()
    }
}
