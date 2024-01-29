//! The `command_core` module contains the core sharing logic for the file sharing system.

use crate::files::FileHandler;
use crate::repl::{QueryResult, Repl, ReplCommand};

use crate::DynResult;
use futures::{AsyncWriteExt, StreamExt};
use libp2p::{
    core::transport::upgrade::Version,
    identity, mdns,
    multiaddr::Protocol,
    request_response::{self, OutboundRequestId, ProtocolSupport /*, ResponseChannel */},
    swarm::{NetworkBehaviour, Swarm, SwarmEvent},
    Multiaddr, PeerId, Stream, StreamProtocol, SwarmBuilder, Transport,
};
use libp2p_stream as stream;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{
    mpsc::{channel, Receiver},
    oneshot,
};

/// The protocol id for the file transfer part of the protocol.
const TRANSFER_PROTOCOL: StreamProtocol =
    StreamProtocol::new("/takehome_libp2p_sharing_transfer/1.0.0");

/// The protocol id for the request/response part of the protocol.
const REQUEST_PROTOCOL: StreamProtocol = StreamProtocol::new("/takehome_libp2p_sharing/1.0.0");

/// The commands that the [CommandCore] can send to a peer on the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum FileRequestCommand {
    /// Requests a list of files.
    ListFiles,

    /// Requests a true/false for the named file.
    FileExists(String),
}

/// The request struct sent over the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileRequest(FileRequestCommand);

/// The responses the [CommandCore] can send to a peer on the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum FileResponseCommand {
    /// A list of file names.  These do not include the full path to the file on the local syste.
    FileList(Vec<String>),

    /// True or false indicating the file exists or not.
    FileExists(bool),
}

/// The response struct sent over the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileResponse(FileResponseCommand);

/// The `libp2p` network behavior for the file sharing system.
#[derive(NetworkBehaviour)]
struct CommandCoreNetworkBehavior {
    /// Use request/response for getting the list of files from a peer and
    /// for checking if a peer has a named file.
    request_response: request_response::cbor::Behaviour<FileRequest, FileResponse>,

    /// Use MDNS for peer discovery.   The exercise specifies 'local' networks, making
    /// MDNS sufficient.
    mdns: mdns::tokio::Behaviour,

    /// Use a stream for the file transfer and the transfer of the file digest.
    stream: stream::Behaviour,
}

/// The event processing loop responds to these behavior flags.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandCoreBehavior {
    /// Keep processing events
    Continue,

    /// Stop the event processing loop.
    Quit,
}

/// The core object that processes events for the File Sharing system.
pub struct CommandCore {
    /// When the [Repl] is configured, use this channel to send messages to the [Repl].
    repl_command_receiver: Option<Receiver<ReplCommand>>,

    /// A file handler object that manages the files on the local system.
    file_handler: FileHandler,

    /// The `libp2p` swarm object.
    swarm: Swarm<CommandCoreNetworkBehavior>,

    /// A map of peer ids to their addresses. We allow a list of addresses for the case of multi-homed machines
    /// that are communicating with a peer on the same machine.
    peer_address_map: HashMap<PeerId, Vec<Multiaddr>>,

    /// A map of request ids to the response channels the [CommandCore] should use to send the response results.
    outstanding_requests: HashMap<OutboundRequestId, oneshot::Sender<DynResult<QueryResult>>>,

    /// A map of peer ids to the channels the [CommandCore] should use to send the dial results.
    outstanding_dial_events: HashMap<PeerId, oneshot::Sender<DynResult<()>>>,
}

impl CommandCore {
    /// Create a new [CommandCore] object.
    ///
    /// # Arguments
    ///
    /// * `repl_command_receiver` - An Option containing the channel to send messages to the [Repl].
    /// * `file_path` - The path to the directory from which to serve the files.
    pub fn new(
        repl_command_receiver: Option<Receiver<ReplCommand>>,
        file_path: &str,
    ) -> DynResult<CommandCore> {
        let keys = identity::Keypair::generate_ed25519();

        let mut swarm = SwarmBuilder::with_existing_identity(keys.clone())
            .with_tokio()
            .with_other_transport(|_| {
                // The parameters for the transport are not tuned in any fine detail.
                let base_transport = libp2p::tcp::tokio::Transport::new(
                    libp2p::tcp::Config::new()
                        .ttl(30)
                        .listen_backlog(5)
                        .nodelay(false),
                );

                // Use noise instead of TLS just to save time.
                let noise_config = libp2p::noise::Config::new(&keys)?;
                let yamux_config = libp2p::yamux::Config::default();

                Ok(base_transport
                    .upgrade(Version::V1)
                    .authenticate(noise_config)
                    .multiplex(yamux_config))
            })?
            .with_behaviour(|key| {
                let mdns = mdns::tokio::Behaviour::new(
                    mdns::Config::default(),
                    key.public().to_peer_id(),
                )?;

                let request_response = request_response::cbor::Behaviour::new(
                    [(REQUEST_PROTOCOL, ProtocolSupport::Full)],
                    request_response::Config::default(),
                );

                let stream = stream::Behaviour::new();

                Ok(CommandCoreNetworkBehavior {
                    request_response,
                    mdns,
                    stream,
                })
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        // Allow `libp2p` to configure the listening address.
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

        Ok(CommandCore {
            repl_command_receiver,
            file_handler: FileHandler::new(PathBuf::from(file_path)),
            swarm,
            peer_address_map: HashMap::new(),
            outstanding_requests: HashMap::new(),
            outstanding_dial_events: HashMap::new(),
        })
    }

    /// The [CommandCore] needs to handle inbound streams.  This function sets up the
    /// futures for handling incoming stream connections.  For the sake of time, this
    /// code does not attempt to parallelize the incoming stream handling. You can
    /// overload the system by sending multiple requests to the same peer.  According to
    /// the `libp2p` docs the underlying library should drop connections to mitigate some
    /// DOS kinds of behavior.
    async fn setup_inbound_stream_handling(&mut self) -> DynResult<()> {
        let mut inbound_streams = self
            .swarm
            .behaviour()
            .stream
            .new_control()
            .accept(TRANSFER_PROTOCOL)?;

        let served_dir = self.file_handler.get_served_dir();
        // Use a separate [FileHandler] instantiation to avoid trying to share
        // the object from the [CommandCore] with the async closure. Omitted for
        // the sake of time.
        let mut file_handler = FileHandler::new(served_dir);

        tokio::spawn(async move {
            while let Some((peer, mut stream)) = inbound_streams.next().await {
                match file_handler.send_response(&mut stream).await {
                    Ok(_) => {
                        info!("Successfully responded to peer: {}", peer);
                    }
                    Err(e) => {
                        info!("Error sending response to peer: {}", e);
                    }
                }
                stream.close().await.expect("Stream should have closed");
            }
        });

        Ok(())
    }

    /// The primary event loop function for the [CommandCore] object.
    pub async fn run(&mut self) -> DynResult<()> {
        self.setup_inbound_stream_handling().await?;

        loop {
            // I'm not happy with how this looks, but not solving for the sake of time.
            let behavior = if let Some(repl_command_receiver) = &mut self.repl_command_receiver {
                tokio::select! {
                    command = repl_command_receiver.recv() => {
                        if let Some(command) = command {
                            self.handle_repl_command(command).await?
                        } else {
                            CommandCoreBehavior::Continue
                        }
                    }
                    event = self.swarm.next() => {
                        if let Some(event) = event {
                            self.handle_swarm_event(event).await?;
                        }
                        CommandCoreBehavior::Continue
                    }
                }
            } else {
                tokio::select! {
                    event = self.swarm.next() => {
                        if let Some(event) = event {
                            self.handle_swarm_event(event).await?;
                        }
                        CommandCoreBehavior::Continue
                    }
                }
            };

            if behavior == CommandCoreBehavior::Quit {
                break;
            }
        }
        Ok(())
    }

    /// Method to open a new stream to a peer.  Used by the file transfer code when getting a file from a peer.
    async fn open_new_stream(&mut self, peer_id: PeerId) -> DynResult<Stream> {
        // If the user requests an invalid peer id, this code will fail and the system will come down.
        // We do check for peer ids that do not parse in the [Repl], but not handling more robustly for lack of time.
        let peer_address_list = self
            .peer_address_map
            .get(&peer_id)
            .ok_or::<String>("Peer was not in address map".into())?;

        // Not gracefully handling the case of an empty address list for the sake of time.
        let peer_address = peer_address_list
            .first()
            .ok_or::<String>("Peer address list was empty".into())?;

        // Make sure we dial the peer.
        self.swarm.dial(peer_address.clone())?;

        let mut control = self.swarm.behaviour().stream.new_control();
        let stream = match control.open_stream(peer_id, TRANSFER_PROTOCOL).await {
            Ok(stream) => stream,
            Err(error @ stream::OpenStreamError::UnsupportedProtocol(_)) => {
                return Err(format!(
                    "Peer {} does not support the file transfer protocol: {}",
                    peer_id, error
                )
                .into());
            }
            Err(e) => {
                return Err(format!("Error opening stream to peer {}: {}", peer_id, e).into());
            }
        };
        Ok(stream)
    }

    async fn handle_repl_command(
        &mut self,
        command: ReplCommand,
    ) -> DynResult<CommandCoreBehavior> {
        match command {
            ReplCommand::Dial(peer_id, repl_tx) => {
                if let Some(peer_address_list) = self.peer_address_map.get(&peer_id) {
                    if let Some(peer_address) = peer_address_list.first() {
                        match self
                            .swarm
                            .dial(peer_address.clone().with(Protocol::P2p(peer_id)))
                        {
                            Ok(()) => {
                                self.outstanding_dial_events
                                    .insert(peer_id.clone(), repl_tx);
                            }
                            Err(e) => {
                                let _ = repl_tx.send(Err(Box::new(e)));
                            }
                        }
                    } else {
                        let _ =
                            repl_tx.send(Err("Peer did not have a valid address in map".into()));
                    }
                } else {
                    let _ = repl_tx.send(Err("Peer was not in address map".into()));
                }
                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::GetPeerList(tx) => {
                // On multi-homed machines, the discovered_nodes() function will return peers on all the available
                // interfaces. The will all have the same peer id, so use a set to deduplicate.
                let mut peer_set = HashSet::<&PeerId>::new();
                let swarm_peers = self
                    .swarm
                    .behaviour_mut()
                    .mdns
                    .discovered_nodes()
                    .collect::<Vec<_>>();
                for peer in swarm_peers {
                    peer_set.insert(peer);
                }

                let peers = peer_set.iter().map(|p| (*p).clone()).collect::<Vec<_>>();
                if let Err(e) = tx.send(peers) {
                    error!("Error sending peer list to REPL: {:?}", e);
                }
                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::ListLocalFiles(tx) => {
                let files = self.file_handler.list_files().await?;
                if let Err(e) = tx.send(Ok(QueryResult::Files(files))) {
                    error!("Error sending file list to REPL: {:?}", e);
                }
                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::ListPeerFiles(peer_id, repl_tx) => {
                let request_id = self
                    .swarm
                    .behaviour_mut()
                    .request_response
                    .send_request(&peer_id, FileRequest(FileRequestCommand::ListFiles));
                self.outstanding_requests.insert(request_id, repl_tx);
                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::DoesFileExist(peer_id, file_name, tx) => {
                let request_id = self.swarm.behaviour_mut().request_response.send_request(
                    &peer_id,
                    FileRequest(FileRequestCommand::FileExists(file_name.clone())),
                );
                self.outstanding_requests.insert(request_id, tx);
                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::GetFileFromPeer(peer_id, file_name, tx) => {
                // This code assumes that the [Repl] has checked the peer to see that the file exists.
                // We make no attempt to mitigate the case where the peer's file disappears in-between
                // the existence check and the retrieval step.
                info!("Getting {} contents from {}", file_name, peer_id);

                // First get the file contents.
                let mut stream = self.open_new_stream(peer_id.clone()).await?;
                let retrieved_digest = self
                    .file_handler
                    .retrieve_file_from_peer(&file_name, &mut stream)
                    .await?;
                stream.close().await?;

                info!("Getting {} digest from {}", file_name, peer_id);

                // Next get the file digest.  This code makes no attempt to handle a malicious case where the user sends
                // file content for one file and digest info for some other file.  The names are not checked.
                stream = self.open_new_stream(peer_id).await?;
                let transmitted_digest = self
                    .file_handler
                    .retrieve_digest_from_peer(&file_name, &mut stream)
                    .await?;
                stream.close().await?;

                // We are only handling the case where the digests match/not match. Any errors in the transport layer
                // will abort the whole process, and we are not mitigating for that possibility.
                let retrieval_succeeded = if retrieved_digest == transmitted_digest {
                    true
                } else {
                    self.file_handler.remove_file(&file_name).await?;
                    false
                };

                if let Err(e) = tx.send(retrieval_succeeded) {
                    error!("Error sending file download status to REPL: {:?}", e);
                }

                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::GetMyPeerId(tx) => {
                if let Err(e) = tx.send(self.swarm.local_peer_id().clone()) {
                    error!("Error sending peer id to REPL: {:?}", e);
                }
                Ok(CommandCoreBehavior::Continue)
            }
            ReplCommand::Shutdown => Ok(CommandCoreBehavior::Quit),
        }
    }

    /// Respond to an event from the network.  I'm grateful to the example code from the rust-libp2p source repository
    /// that shows how to handle the various events. I have used some of those code patterns here.
    async fn handle_swarm_event(
        &mut self,
        event: SwarmEvent<CommandCoreNetworkBehaviorEvent>,
    ) -> DynResult<()> {
        match event {
            SwarmEvent::Behaviour(CommandCoreNetworkBehaviorEvent::RequestResponse(
                request_response::Event::Message { message, .. },
            )) => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => match request {
                    FileRequest(FileRequestCommand::ListFiles) => {
                        info!("Handling list files request");
                        let files = self.file_handler.list_files().await?;
                        self.swarm
                            .behaviour_mut()
                            .request_response
                            .send_response(
                                channel,
                                FileResponse(FileResponseCommand::FileList(files)),
                            )
                            .expect("Failed to send response");
                    }
                    FileRequest(FileRequestCommand::FileExists(file_name)) => {
                        info!("Handling file exists request");
                        let exists = self.file_handler.file_exists(&file_name).await?;
                        self.swarm
                            .behaviour_mut()
                            .request_response
                            .send_response(
                                channel,
                                FileResponse(FileResponseCommand::FileExists(exists)),
                            )
                            .expect("Failed to send response");
                    }
                },
                request_response::Message::Response {
                    request_id,
                    response,
                } => {
                    info!("Handling network response");
                    let query_result = match response {
                        FileResponse(FileResponseCommand::FileList(files)) => {
                            QueryResult::Files(files)
                        }
                        FileResponse(FileResponseCommand::FileExists(exists)) => {
                            QueryResult::Boolean(exists)
                        }
                    };
                    let _ = self
                        .outstanding_requests
                        .remove(&request_id)
                        .expect("No sender for request id")
                        .send(Ok(query_result));
                }
            },
            SwarmEvent::Behaviour(CommandCoreNetworkBehaviorEvent::RequestResponse(
                request_response::Event::OutboundFailure {
                    request_id, error, ..
                },
            )) => {
                info!("Handling request outbound failure");
                let _ = self
                    .outstanding_requests
                    .remove(&request_id)
                    .expect("No sender for request id")
                    .send(Err(Box::new(error)));
            }
            SwarmEvent::Behaviour(CommandCoreNetworkBehaviorEvent::RequestResponse(
                request_response::Event::ResponseSent { .. },
            )) => {}
            SwarmEvent::NewListenAddr { address, .. } => {
                let this_peer_id = *self.swarm.local_peer_id();
                info!("Listening on {}", address.with(Protocol::P2p(this_peer_id)));
            }
            SwarmEvent::Behaviour(CommandCoreNetworkBehaviorEvent::Mdns(
                mdns::Event::Discovered(list),
            )) => {
                for (peer_id, address) in list {
                    info!("Discovered peer: {} at {}", peer_id, address);
                    if !self.peer_address_map.contains_key(&peer_id) {
                        self.peer_address_map.insert(peer_id.clone(), Vec::new());
                    }
                    let peer_address_list = self.peer_address_map.get_mut(&peer_id).unwrap();
                    peer_address_list.push(address.clone());
                    self.swarm
                        .behaviour_mut()
                        .request_response
                        .add_address(&peer_id, address);
                }
            }
            SwarmEvent::Behaviour(CommandCoreNetworkBehaviorEvent::Mdns(mdns::Event::Expired(
                list,
            ))) => {
                for (peer_id, address) in list {
                    info!("Expired peer: {} at {}", peer_id, address.clone());
                    if let Some(peer_address_list) = self.peer_address_map.get(&peer_id) {
                        let new_list = peer_address_list
                            .iter()
                            .filter(|a| **a != address)
                            .map(|a| a.clone())
                            .collect::<Vec<_>>();
                        self.peer_address_map.insert(peer_id, new_list);
                        self.swarm
                            .behaviour_mut()
                            .request_response
                            .remove_address(&peer_id, &address);
                    }
                }
            }
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                info!(
                    "Connection established to peer: {} at {:?}",
                    peer_id, endpoint
                );
                if endpoint.is_dialer() {
                    if let Some(tx) = self.outstanding_dial_events.remove(&peer_id) {
                        let _ = tx.send(Ok(()));
                    }
                }
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    info!("Outgoing connection error to peer {}: {}", peer_id, error);
                    if let Some(tx) = self.outstanding_dial_events.remove(&peer_id) {
                        let _ = tx.send(Err(Box::new(error)));
                    }
                }
            }
            SwarmEvent::Dialing { peer_id, .. } => {
                if let Some(peer_id) = peer_id {
                    info!("Dialing peer: {}", peer_id);
                }
            }
            SwarmEvent::IncomingConnection { .. } => {}
            SwarmEvent::IncomingConnectionError { .. } => {}
            SwarmEvent::ConnectionClosed { .. } => {}

            _ => {
                info!("Not handling {:?}", event);
            }
        }
        Ok(())
    }
}

/// Function to create a [Repl] and a [CommandCore] object for running in client mode.
///
/// # Arguments
///
/// `file_path` - The path to the directory from which to serve the files.
pub fn setup_client_system(file_path: &str) -> DynResult<(Repl, CommandCore)> {
    let (repl_command_sender, repl_command_receiver) = channel::<ReplCommand>(5);
    let repl = Repl::new(repl_command_sender);
    let core = CommandCore::new(Some(repl_command_receiver), file_path)?;
    Ok((repl, core))
}

/// Function to create just a [CommandCore] object for running in server mode.
///
/// # Arguments
///
/// `file_path` - The path to the directory from which to serve the files.
pub fn setup_file_provider_system(file_path: &str) -> DynResult<CommandCore> {
    let core = CommandCore::new(None, file_path)?;
    Ok(core)
}
