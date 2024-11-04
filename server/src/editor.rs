use std::{collections::HashMap, fmt::Debug, net::SocketAddr};

use axum::extract::ws::Message;
use axum_prometheus::metrics::gauge;
use futures::{sink::SinkExt, stream::StreamExt, Sink, Stream};

use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};
use tracing::Instrument;
use uuid::Uuid;
use yrs::{
    encoding::{read::Cursor, write::Write},
    sync::{
        protocol::{MSG_SYNC, MSG_SYNC_UPDATE},
        Awareness, DefaultProtocol, MessageReader, Protocol,
    },
    updates::{
        decoder::DecoderV1,
        encoder::{Encode, Encoder, EncoderV1},
    },
    Doc,
};

// Websocket

/// A generic wrapper around `axum::extract::ws::WebSocket`.
#[derive(Debug)]
pub struct Connection<Sender, Receiver> {
    pub sender: Sender,
    pub receiver: Receiver,
}

impl<Sender, Receiver> Connection<Sender, Receiver>
where
    Sender: Sink<Message> + Unpin,
    Receiver: Stream<Item = Result<Message, axum::Error>> + Unpin,
{
    async fn new(sender: Sender, receiver: Receiver) -> Self
    where
        Sender: Sink<Message> + Unpin,
        Receiver: Stream<Item = Result<Message, axum::Error>> + Unpin,
    {
        Self { sender, receiver }
    }
}

// Client

pub type ClientId = Uuid;

#[derive(Debug)]
pub struct ClientHandle {
    id: ClientId,
    ip: SocketAddr,
    server_sender: mpsc::Sender<FromServerMessage>,
    join: JoinHandle<()>,
}

impl ClientHandle {
    pub fn send(&mut self, msg: FromServerMessage) -> Result<(), mpsc::error::TrySendError<FromServerMessage>> {
        let res = self.server_sender.try_send(msg);
        match res {
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        }
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        tracing::trace!("drop client");
        gauge!("editor.connections.count").decrement(1);
        self.join.abort()
    }
}

#[derive(Debug)]
pub struct ClientParams<Sender, Receiver> {
    pub id: ClientId,
    pub ip: SocketAddr,
    pub server_handle: ServerHandle,
    pub connection: Connection<Sender, Receiver>,
    pub capacity: Option<usize>,
}

/// Iternal actor data
pub struct ClientData<Sender, Receiver> {
    pub id: ClientId,
    pub server_handle: ServerHandle,
    server_receiver: mpsc::Receiver<FromServerMessage>,
    broadcast_receiver: broadcast::Receiver<BroadcastMessage>,
    pub connection: Connection<Sender, Receiver>,
}

#[tracing::instrument(name = "client", skip(params), fields(?ip = params.ip, ?id = params.id))]
pub fn spawn_client<Sender, Receiver>(params: ClientParams<Sender, Receiver>)
where
    Sender: Sink<Message> + Send + Sync + Unpin + 'static + Debug,
    Receiver: Stream<Item = Result<Message, axum::Error>> + Send + Sync + Unpin + 'static + Debug,
{
    tracing::trace!("client: spawn");
    gauge!("editor.connections.count").increment(1);

    let (server_sender, server_receiver) = mpsc::channel(params.capacity.unwrap_or(100));

    // Iternal actor data
    let mut data = ClientData {
        id: params.id,
        server_handle: params.server_handle.clone(),
        server_receiver,
        broadcast_receiver: params.server_handle.subscribe(),
        connection: params.connection,
    };

    // This spawns the new task.
    let (handle_sender, handle_receiver) = oneshot::channel();
    let kill_handle = tokio::spawn(async move {
        let handle = match handle_receiver.await {
            Ok(handle) => handle,
            Err(_) => return,
        };
        data.server_handle.send(ToServerMessage::Join(handle)).await;

        // Websocket connection loop
        let mut connection_receiver = data.connection.receiver;
        let mut server_handle = data.server_handle.clone();
        let mut connection_receive_loop = tokio::spawn(async move {
            tracing::debug!("client: run ws receive loop");
            while let Some(result) = connection_receiver.next().await {
                match result {
                    Ok(Message::Binary(input)) => {
                        tracing::debug!("client: receive ws binary message");
                        _ = server_handle.send(ToServerMessage::Message(data.id, input)).await;
                    }
                    Ok(message) => {
                        tracing::debug!("client: receive other message: {message:?}");
                    }
                    Err(e) => {
                        tracing::error!("client: ws receive error: {:?}", e);
                        break;
                    }
                }
            }
        });
        let mut connection_sender = data.connection.sender;
        let mut connection_send_loop = tokio::spawn(async move {
            tracing::debug!("client: run ws send loop");
            while let Ok(BroadcastMessage::Binary { payload }) = data.broadcast_receiver.recv().await {
                if let Err(e) = connection_sender.send(Message::Binary(payload)).await {
                    tracing::error!("client: ws send error");
                    break;
                }
            }

            while let Some(message) = data.server_receiver.recv().await {
                match message {
                    FromServerMessage::Binary(data) => {
                        tracing::debug!("client: send ws binary message");

                        if let Err(e) = connection_sender.send(Message::Binary(data)).await {
                            tracing::error!("client: ws send error");
                            break;
                        }
                    }
                };
            }
        });
        _ = tokio::select! {
            _ = &mut connection_send_loop => {
                tracing::trace!("client: abort connection_receive_loop");
                connection_receive_loop.abort()
            },
            _ = &mut connection_receive_loop => {
                tracing::trace!("client: abort connection_send_loop");
                connection_send_loop.abort()
            },
        };

        tracing::debug!("client: leave");
        data.server_handle.send(ToServerMessage::Leave(data.id)).await;
    });

    // Then we create a ClientHandle to this new task, and use the oneshot
    // channel to send it to the task.
    let handle = ClientHandle {
        id: params.id,
        ip: params.ip,
        server_sender,
        join: kill_handle,
    };

    let _ = handle_sender.send(handle);
}

// Server

#[derive(Debug, Clone)]
pub struct ServerHandle {
    sender: mpsc::Sender<ToServerMessage>,
    bsender: broadcast::Sender<BroadcastMessage>,
}

impl ServerHandle {
    pub async fn send(&mut self, msg: ToServerMessage) {
        if self.sender.send(msg).await.is_err() {
            panic!("Main loop has shut down.");
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastMessage> {
        self.bsender.subscribe()
    }
}

pub struct ServerParams {
    pub capacity: Option<usize>,
}

#[derive(Default, Debug)]
struct ServerData {
    clients: HashMap<ClientId, ClientHandle>,
    awareness: Awareness,
}

pub enum ToServerMessage {
    Join(ClientHandle),
    Leave(ClientId),
    Message(ClientId, Vec<u8>),
    Stop,
}

#[derive(Debug, Clone)]
pub enum BroadcastMessage {
    Binary { payload: Vec<u8> },
}

#[derive(Debug, Clone)]
pub enum FromServerMessage {
    Binary(Vec<u8>),
}

#[tracing::instrument(name="server", skip(params), fields(capacity = params.capacity))]
pub fn spawn_server(params: ServerParams) -> (ServerHandle, JoinHandle<()>) {
    tracing::debug!("server: spawn");

    let (bsender, _) = broadcast::channel(params.capacity.unwrap_or(100));
    let (sender, mut receiver) = mpsc::channel(params.capacity.unwrap_or(100));

    let doc_broadcast_sender = bsender.clone();
    let awareness_broadcast_sender = bsender.clone();
    let handle = ServerHandle { sender, bsender };

    let join = tokio::spawn(
        async move {
            let doc = Doc::new();

            let awareness = Awareness::new(doc);
            let protocol = DefaultProtocol;

            let mut data = ServerData {
                clients: HashMap::default(),
                awareness,
            };

            let (doc_sub, awareness_sub) = {
                let doc_sub = data
                    .awareness
                    .doc_mut()
                    .observe_update_v1(move |_txn, u| {
                        let mut encoder = EncoderV1::new();
                        encoder.write_var(MSG_SYNC);
                        encoder.write_var(MSG_SYNC_UPDATE);
                        encoder.write_buf(&u.update);
                        let payload = encoder.to_vec();

                        tracing::debug!("server: broadcast docs");
                        let res = doc_broadcast_sender.send(BroadcastMessage::Binary { payload });
                    })
                    .unwrap();

                let awareness_sub = data.awareness.on_update(move |awareness, e, origin| {
                    let added = e.added();
                    let updated = e.updated();
                    let removed = e.removed();
                    let mut changed = Vec::with_capacity(added.len() + updated.len() + removed.len());
                    changed.extend_from_slice(added);
                    changed.extend_from_slice(updated);
                    changed.extend_from_slice(removed);

                    if let Ok(u) = awareness.update_with_clients(changed) {
                        let payload = yrs::sync::Message::Awareness(u).encode_v1();

                        tracing::debug!("server: broadcast awareness");
                        let res = awareness_broadcast_sender.send(BroadcastMessage::Binary { payload });
                    }
                });
                (doc_sub, awareness_sub)
            };

            tracing::debug!("server: run loop");
            while let Some(message) = receiver.recv().in_current_span().await {
                match message {
                    ToServerMessage::Join(mut client_handle) => {
                        tracing::debug!("server: new client {:?}", client_handle.id);

                        let encoder = EncoderV1::new();
                        let payload = encoder.to_vec();
                        if !payload.is_empty() {
                            _ = client_handle.send(FromServerMessage::Binary(payload));
                        }

                        data.clients.insert(client_handle.id, client_handle);
                    }
                    ToServerMessage::Message(from_id, input) => {
                        tracing::debug!("server: got message from: {:?}", from_id);

                        {
                            let mut decoder = DecoderV1::new(Cursor::new(&input));
                            let reader = MessageReader::new(&mut decoder);
                            let dbg_msgs = reader.collect::<Vec<_>>();
                            tracing::trace!("server: input messages: {:?}", dbg_msgs);
                        }

                        let replies = protocol.handle(&data.awareness, &input);
                        let client_handle = data.clients.get_mut(&from_id).unwrap();

                        if let Ok(replies) = replies {
                            tracing::debug!("server: reply to {:?}", from_id);
                            for reply in replies {
                                tracing::trace!("server: output message: {:?}", reply);
                                _ = client_handle.send(FromServerMessage::Binary(reply.encode_v1()));
                            }
                        }
                    }
                    ToServerMessage::Leave(id) => {
                        tracing::debug!("server: remove client: {:?}", id);
                        data.clients.remove(&id);
                    }
                    ToServerMessage::Stop => {
                        break;
                    }
                }
            }
        }
        .in_current_span(),
    );

    (handle, join)
}

#[cfg(test)]
mod tests {}
