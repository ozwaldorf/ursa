//! Ursa Discovery implementation.

use std::borrow::Cow;
use std::ops::Not;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroUsize,
    task::{Context, Poll},
};

use crate::config::NetworkConfig;
use anyhow::{anyhow, Error, Result};
use futures_timer::Delay;
use futures_util::FutureExt;
use libp2p::kad::BootstrapOk;
use libp2p::mdns::tokio::Behaviour as Mdns;
use libp2p::swarm::derive_prelude::FromSwarm;
use libp2p::{
    core::connection::ConnectionId,
    identity::Keypair,
    kad::{
        handler::KademliaHandlerProto, store::MemoryStore, Kademlia, KademliaConfig, KademliaEvent,
        QueryId, QueryResult,
    },
    mdns::Event as MdnsEvent,
    multiaddr::Protocol,
    swarm::{
        behaviour::toggle::Toggle, ConnectionHandler, IntoConnectionHandler, NetworkBehaviour,
        NetworkBehaviourAction, PollParameters,
    },
    Multiaddr, PeerId,
};
use tracing::{info, warn};
use ursa_metrics::Recorder;

pub const URSA_KAD_PROTOCOL: &[u8] = b"/ursa/kad/0.0.1";
const INITIAL_BOOTSTRAP_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum DiscoveryEvent {
    Connected(PeerId),
    Disconnected(PeerId),
}

pub struct DiscoveryBehaviour {
    /// Kademlia instance.
    kademlia: Kademlia<MemoryStore>,
    /// Boostrap nodes.
    bootstrap_nodes: Vec<(PeerId, Multiaddr)>,
    /// Connected peers.
    peers: HashSet<PeerId>,
    /// Information about connected peers.
    peer_info: HashMap<PeerId, Vec<Multiaddr>>,
    /// events
    events: VecDeque<DiscoveryEvent>,
    /// Optional MDNS protocol.
    mdns: Toggle<Mdns>,
    /// Delay until kad bootstrap
    next_bootstrap: Option<Delay>,
    /// Bootstrap interval
    bootstrap_interval: Duration,
}

impl DiscoveryBehaviour {
    pub fn new(keypair: &Keypair, config: &NetworkConfig) -> Self {
        let mut peers = HashSet::new();
        let local_peer_id = PeerId::from(keypair.public());

        let bootstrap_nodes: Vec<(PeerId, Multiaddr)> = config
            .bootstrap_nodes
            .clone()
            .into_iter()
            .filter_map(|multiaddr| {
                let mut addr = multiaddr.to_owned();
                if let Some(Protocol::P2p(mh)) = addr.pop() {
                    let peer_id = PeerId::from_multihash(mh).unwrap();
                    Some((peer_id, addr))
                } else {
                    warn!("Could not parse bootstrap addr {}", multiaddr);
                    None
                }
            })
            .collect();

        // Kickoff bootstraps with initial delay of 10s if we are not a bootstrapper
        let next_bootstrap = config
            .bootstrapper
            .not()
            .then_some(Delay::new(INITIAL_BOOTSTRAP_DELAY));

        // setup kademlia config
        let mut kademlia = {
            let store = MemoryStore::new(local_peer_id);
            // todo(botch): move replication factor to config
            let replication_factor = NonZeroUsize::new(8).unwrap();
            let mut kad_config = KademliaConfig::default();
            kad_config
                .set_protocol_names(vec![Cow::from(URSA_KAD_PROTOCOL)])
                .set_replication_factor(replication_factor);

            Kademlia::with_config(local_peer_id, store, kad_config.clone())
        };

        for (peer_id, address) in bootstrap_nodes.clone() {
            kademlia.add_address(&peer_id, address.clone());
            peers.insert(peer_id);
        }

        let mdns = config
            .mdns
            .then_some(Mdns::new(Default::default()).expect("mDNS start"));

        Self {
            kademlia,
            bootstrap_nodes,
            peers,
            peer_info: HashMap::new(),
            events: VecDeque::new(),
            mdns: mdns.into(),
            next_bootstrap,
            bootstrap_interval: Duration::from_secs(config.bootstrap_interval),
        }
    }

    pub fn add_address(&mut self, peer_id: &PeerId, address: Multiaddr) {
        self.kademlia.add_address(peer_id, address);
    }

    pub fn peers(&self) -> &HashSet<PeerId> {
        &self.peers
    }

    pub fn peer_info(&self) -> &HashMap<PeerId, Vec<Multiaddr>> {
        &self.peer_info
    }

    pub fn bootstrap(&mut self) -> Result<QueryId, Error> {
        info!("Initiating bootstrap");
        self.kademlia
            .bootstrap()
            .map_err(|err| anyhow!("{:?}", err))
    }

    pub fn bootstrap_addrs(&self) -> Vec<(PeerId, Multiaddr)> {
        self.bootstrap_nodes.clone()
    }

    fn handle_kad_event(&mut self, event: KademliaEvent) {
        match event {
            KademliaEvent::OutboundQueryProgressed {
                result: QueryResult::Bootstrap(res),
                ..
            } => match res {
                Ok(BootstrapOk { num_remaining, .. }) => {
                    if num_remaining == 0 {
                        info!("[KademliaEvent] Bootstrap complete");
                        self.next_bootstrap = Some(Delay::new(self.bootstrap_interval));
                    }
                }
                Err(e) => {
                    warn!("[KademliaEvent] Bootstrap failed: {:?}", e);
                    self.next_bootstrap = Some(Delay::new(self.bootstrap_interval * 2));
                }
            },
            KademliaEvent::RoutingUpdated {
                peer, is_new_peer, ..
            } => {
                if is_new_peer {
                    info!("[KademliaEvent] Routing updated for new peer: {}", peer);
                }
            }
            e => info!("[KademliaEvent] {:?}", e),
        }
    }

    fn handle_mdns_event(&mut self, event: MdnsEvent) {
        match event {
            MdnsEvent::Discovered(discovered_peers) => {
                for (peer_id, address) in discovered_peers {
                    self.add_address(&peer_id, address)
                }
            }
            MdnsEvent::Expired(_) => {}
        }
    }
}

impl NetworkBehaviour for DiscoveryBehaviour {
    type ConnectionHandler = KademliaHandlerProto<QueryId>;

    type OutEvent = DiscoveryEvent;

    fn new_handler(&mut self) -> Self::ConnectionHandler {
        self.kademlia.new_handler()
    }

    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        let mut addrs = Vec::new();
        addrs.extend(self.kademlia.addresses_of_peer(peer_id));
        addrs.extend(self.mdns.addresses_of_peer(peer_id));
        addrs
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionEstablished(event) => {
                self.peers.insert(event.peer_id);

                let addresses_of_peer = self.addresses_of_peer(&event.peer_id);
                self.peer_info.insert(event.peer_id, addresses_of_peer);

                self.events
                    .push_back(DiscoveryEvent::Connected(event.peer_id));
            }
            FromSwarm::ConnectionClosed(event) => {
                self.peers.remove(&event.peer_id);
                self.events
                    .push_back(DiscoveryEvent::Disconnected(event.peer_id));
            }
            _ => {}
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        event: <<Self::ConnectionHandler as IntoConnectionHandler>::Handler as
        ConnectionHandler>::OutEvent,
    ) {
        self.kademlia
            .on_connection_handler_event(peer_id, connection_id, event)
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        params: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ConnectionHandler>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(event));
        }

        // Poll kademlia for events
        while let Poll::Ready(action) = self.kademlia.poll(cx, params) {
            match action {
                NetworkBehaviourAction::GenerateEvent(event) => {
                    event.record();
                    self.handle_kad_event(event)
                }
                NetworkBehaviourAction::Dial { opts, handler } => {
                    return Poll::Ready(NetworkBehaviourAction::Dial { opts, handler })
                }
                NetworkBehaviourAction::NotifyHandler {
                    peer_id,
                    handler,
                    event,
                } => {
                    return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                        peer_id,
                        handler,
                        event,
                    })
                }
                NetworkBehaviourAction::ReportObservedAddr { address, score } => {
                    return Poll::Ready(NetworkBehaviourAction::ReportObservedAddr {
                        address,
                        score,
                    })
                }
                NetworkBehaviourAction::CloseConnection {
                    peer_id,
                    connection,
                } => {
                    return Poll::Ready(NetworkBehaviourAction::CloseConnection {
                        peer_id,
                        connection,
                    })
                }
            }
        }

        // Run kademlia bootstraps periodically (if timer is set)
        if let Some(delay) = self.next_bootstrap.as_mut() {
            // if connected peers are under 12, and timer is ready
            if self.peers.len() < 12 && delay.poll_unpin(cx).is_ready() {
                if let Err(e) = self.bootstrap() {
                    warn!("Bootstrap failed: {:?}", e);
                    // 2x longer delay if bootstrap failed
                    self.next_bootstrap = Some(Delay::new(self.bootstrap_interval * 2));
                } else {
                    self.next_bootstrap = None;
                }
            }
        }

        // Poll mdns for events
        while let Poll::Ready(action) = self.mdns.poll(cx, params) {
            match action {
                NetworkBehaviourAction::GenerateEvent(event) => self.handle_mdns_event(event),
                NetworkBehaviourAction::ReportObservedAddr { address, score } => {
                    return Poll::Ready(NetworkBehaviourAction::ReportObservedAddr {
                        address,
                        score,
                    })
                }
                NetworkBehaviourAction::CloseConnection {
                    peer_id,
                    connection,
                } => {
                    return Poll::Ready(NetworkBehaviourAction::CloseConnection {
                        peer_id,
                        connection,
                    })
                }
                NetworkBehaviourAction::Dial { .. }
                | NetworkBehaviourAction::NotifyHandler { .. } => {}
            }
        }

        Poll::Pending
    }
}
