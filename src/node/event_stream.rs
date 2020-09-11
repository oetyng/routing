// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{
    error::Result,
    event::{Connected, Event},
    location::DstLocation,
    messages::Message,
    node::stage::Stage,
};
use bytes::Bytes;
use futures::lock::Mutex;
use qp2p::{IncomingConnections, IncomingMessages, Message as QuicP2pMsg};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::mpsc;
use xor_name::XorName;

// Maximum number of events to be buffered internally, when the buffer is full
// no new events can be generated by this crate
// TODO: if external connections or messages are arriving when
// the buffer is full, they need to be rejected.
const MAX_EVENTS_BUFFERED: usize = 1024;

/// Stream of routing node events
pub struct EventStream {
    events_rx: mpsc::Receiver<Event>,
}

impl EventStream {
    pub(crate) async fn new(
        stage: Arc<Mutex<Stage>>,
        xorname: XorName,
        is_genesis: bool,
    ) -> Result<Self> {
        let incoming_conns = stage.lock().await.listen_events()?;
        let (events_tx, events_rx) = mpsc::channel::<Event>(MAX_EVENTS_BUFFERED);
        Self::spawn_connections_handler(stage, events_tx, incoming_conns, xorname, is_genesis);

        Ok(Self { events_rx })
    }

    /// Returns next event
    pub async fn next(&mut self) -> Option<Event> {
        self.events_rx.recv().await
    }

    // Spawns a task which handles each new incoming connection from peers
    fn spawn_connections_handler(
        stage: Arc<Mutex<Stage>>,
        mut events_tx: mpsc::Sender<Event>,
        mut incoming_conns: IncomingConnections,
        xorname: XorName,
        is_genesis: bool,
    ) {
        let _ = tokio::spawn(async move {
            if is_genesis {
                if let Err(err) = events_tx.send(Event::Connected(Connected::First)).await {
                    trace!("Error reporting new Event: {:?}", err);
                }
                if let Err(err) = events_tx.send(Event::PromotedToElder).await {
                    trace!("Error reporting new Event: {:?}", err);
                }
            }

            while let Some(incoming_msgs) = incoming_conns.next().await {
                trace!(
                    "New connection established by peer {}",
                    incoming_msgs.remote_addr()
                );
                Self::spawn_messages_handler(
                    stage.clone(),
                    events_tx.clone(),
                    incoming_msgs,
                    xorname,
                )
            }
        });
    }

    // Spawns a task which handles each new incoming message from a connection with a peer
    fn spawn_messages_handler(
        stage: Arc<Mutex<Stage>>,
        mut events_tx: mpsc::Sender<Event>,
        mut incoming_msgs: IncomingMessages,
        xorname: XorName,
    ) {
        let _ = tokio::spawn(async move {
            while let Some(msg) = incoming_msgs.next().await {
                match msg {
                    QuicP2pMsg::UniStream { bytes, src, .. } => {
                        trace!(
                            "New message ({} bytes) received on a uni-stream from: {}",
                            bytes.len(),
                            src
                        );
                        // Since it's arriving on a uni-stream we treat it as a Node
                        // message which we need to be processed by us, as well as
                        // reported to the event stream consumer.
                        spawn_node_message_handler(stage.clone(), events_tx.clone(), bytes, src);
                    }
                    QuicP2pMsg::BiStream {
                        bytes,
                        src,
                        send,
                        recv,
                    } => {
                        trace!(
                            "New message ({} bytes) received on a bi-stream from: {}",
                            bytes.len(),
                            src
                        );

                        // Since it's arriving on a bi-stream we treat it as a Client
                        // message which we report directly to the event stream consumer
                        // without doing any intermediate processing.
                        let event = Event::ClientMessageReceived {
                            content: bytes,
                            src,
                            dst: DstLocation::Node(xorname),
                            send,
                            recv,
                        };

                        if let Err(err) = events_tx.send(event).await {
                            trace!("Error reporting new Event: {:?}", err);
                        }
                    }
                }
            }
        });
    }
}

fn spawn_node_message_handler(
    stage: Arc<Mutex<Stage>>,
    mut events_tx: mpsc::Sender<Event>,
    msg_bytes: Bytes,
    sender: SocketAddr,
) {
    let _ = tokio::spawn(async move {
        match Message::from_bytes(&msg_bytes) {
            Err(error) => {
                debug!("Failed to deserialize message: {:?}", error);
            }
            Ok(msg) => {
                trace!("try handle message {:?}", msg);
                // Process the message according to our stage
                if let Err(err) = stage
                    .lock()
                    .await
                    .process_message(sender, msg.clone(), &mut events_tx)
                    .await
                {
                    error!(
                        "Error encountered when processing message {:?}: {}",
                        msg, err
                    );
                }
            }
        }
    });
}
