use super::ChannelIndex;
use crate::webrtc_socket::MatchboxDataChannel;
use crate::Packet;
use futures::{Sink, Stream, StreamExt};
use futures_channel::mpsc::{unbounded, SendError, UnboundedReceiver, UnboundedSender};
use matchbox_protocol::PeerId;
use parking_lot::Mutex;
use std::{collections::HashMap, mem::replace, pin::Pin, sync::Arc, task::Poll};

/// A simple SimpleWebRtc API which can be destructured into a transmitter and a receiver.
pub struct SimpleWebRtc {
    pub rx: SimpleWebRtcRx,
    pub tx: SimpleWebRtcTx,
}

/// A simple SimpleWebRtc transmitter.
///
/// Which Peers exist here is based on the events which have been observed through the corresponding [SimpleWebRtc.rx].
pub struct SimpleWebRtcTx {
    pub(crate) state: TxStateRef,
}

type TxStateRef = Arc<Mutex<HashMap<PeerId, Vec<DataChannelTx>>>>;

pub(crate) enum DataChannelTx {
    /// Buffer low channel.
    /// Left behind when taking sink to inform sink when it should send more.
    Taken(UnboundedSender<()>),
    DataChannel(Box<dyn MatchboxDataChannel>),
}

impl DataChannelTx {
    pub(crate) fn take(&mut self) -> Result<DataChannelSink, TakeError> {
        if matches!(self, DataChannelTx::DataChannel(_)) {
            let (buffer_low_tx, buffer_low_rx) = unbounded::<()>();
            let old = replace(self, DataChannelTx::Taken(buffer_low_tx));
            if let DataChannelTx::DataChannel(c) = old {
                Ok(DataChannelSink {
                    buffer_low: buffer_low_rx,
                    channel: c,
                })
            } else {
                panic!()
            }
        } else {
            Err(TakeError::AlreadyTaken)
        }
    }
}

pub enum TakeError {
    AlreadyTaken,
    InvalidPeer,
    InvalidChannel,
}

pub enum SimpleWebRtcEvent {
    /// First event, fired only once
    IdAssigned(PeerId),
    /// A new peer has connected.
    /// It can now be taken or sent messages via the [SimpleWebRtc.tx]
    PeerConnected(PeerId),
    /// A new peer has disconnected.
    /// It can no longer be taken or sent messages via the [SimpleWebRtc.tx]
    PeerDisconnected(PeerId),
    /// Packet received from peer.
    Packet(PeerId, ChannelIndex, Packet),
    /// The specified DataChannel's output buffer has reached its low threshold.
    TransmitBufferLow(PeerId, ChannelIndex),
}

pub enum SimpleWebRtcClosedReason {
    Error(String),
    Closed,
}

impl SimpleWebRtcTx {
    /// Gets a sink for writing to a given peer's channel.
    ///
    /// This removes this sink from being writable
    pub fn take_sink(
        &mut self,
        peer: PeerId,
        channel: ChannelIndex,
        // TODO: Sink Error type
    ) -> Result<DataChannelSink, TakeError> {
        let mut locked = self.state.lock();
        let peer_data = locked.get_mut(&peer);
        match peer_data {
            Some(v) => match v.get_mut(channel.0) {
                Some(opt) => opt.take(),
                None => Err(TakeError::InvalidChannel),
            },
            None => Err(TakeError::InvalidPeer),
        }
    }

    /// TODO: on success, return send buffer size for DataChannel
    pub fn send(
        &mut self,
        peer: PeerId,
        channel: ChannelIndex,
        packet: Packet,
    ) -> Result<(), TakeError> {
        let mut locked = self.state.lock();
        let peer_data = locked.get_mut(&peer);
        match peer_data {
            Some(v) => match v.get_mut(channel.0) {
                Some(opt) => match opt {
                    DataChannelTx::DataChannel(ref mut tx) => {
                        tx.send(packet);
                        Ok(())
                    }
                    DataChannelTx::Taken(_) => Err(TakeError::AlreadyTaken),
                },
                None => Err(TakeError::InvalidChannel),
            },
            None => Err(TakeError::InvalidPeer),
        }
    }

    // TODO: entry style API for DataChannel:
    // fn get(channel,Peer) ->DataChannelGuard
    // implement take, send, buffer size, etc.
}

pub(crate) struct DataChannelSink {
    pub(crate) channel: Box<dyn MatchboxDataChannel>,
    pub(crate) buffer_low: UnboundedReceiver<()>,
}

impl Sink<Packet> for DataChannelSink {
    type Error = SendError;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn start_send(self: Pin<&mut Self>, item: Packet) -> Result<(), Self::Error> {
        todo!()
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        todo!()
    }
}

/// A simple SimpleWebRtc transmitter.
///
/// Which Peers exist here is based on the events which have been observed through the corresponding [SimpleWebRtc.rx].
pub struct SimpleWebRtcRx {
    rx: UnboundedReceiver<SimpleWebRtcEvent>,
    tx: TxStateRef,
    /// Streams which have been "taken" and who's events are excluded from this object output.
    taken: HashMap<(PeerId, ChannelIndex), UnboundedSender<Packet>>,
}

pub(crate) struct DataChannelStream {
    pub(crate) rx: UnboundedReceiver<Packet>,
}

impl SimpleWebRtcRx {
    pub fn take_stream(
        &mut self,
        peer: PeerId,
        channel: ChannelIndex,
    ) -> Result<DataChannelStream, TakeError> {
        // TODO: validate peer and channel exist

        let entry = self.taken.entry((peer, channel));
        match entry {
            std::collections::hash_map::Entry::Occupied(_) => Err(TakeError::AlreadyTaken),
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                let (tx, rx) = unbounded::<Packet>();
                vacant_entry.insert(tx);
                Ok(DataChannelStream { rx })
            }
        }
    }

    pub async fn read(&mut self) -> Option<SimpleWebRtcEvent> {
        loop {
            let read = self.rx.next().await;

            // TODO: forward taken events. Forward buffer low events.

            return read;
        }
    }
}

impl Stream for SimpleWebRtcRx {
    type Item = SimpleWebRtcEvent;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        todo!()
    }
}

impl Stream for DataChannelStream {
    type Item = Packet;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        todo!()
    }
}
