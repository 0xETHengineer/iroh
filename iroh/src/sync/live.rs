use std::{collections::HashMap, fmt, net::SocketAddr, str::FromStr, sync::Arc};

use crate::sync::connect_and_sync;
use anyhow::{anyhow, Result};
use futures::{
    future::{BoxFuture, Shared},
    stream::{BoxStream, FuturesUnordered, StreamExt},
    FutureExt, TryFutureExt,
};
use iroh_bytes::util::runtime::Handle;
use iroh_gossip::{
    net::{Event, Gossip},
    proto::TopicId,
};
use iroh_metrics::inc;
use iroh_net::{tls::PeerId, MagicEndpoint};
use iroh_sync::{
    store,
    sync::{InsertOrigin, Replica, SignedEntry},
};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, task::JoinError};
use tracing::{debug, error};

use super::metrics::Metrics;

const CHANNEL_CAP: usize = 8;

/// The address to connect to a peer
/// TODO: Move into iroh-net
/// TODO: Make an enum and support DNS resolution
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerSource {
    pub peer_id: PeerId,
    pub addrs: Vec<SocketAddr>,
    pub derp_region: Option<u16>,
}

impl PeerSource {
    /// Deserializes from bytes.
    fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        postcard::from_bytes(bytes).map_err(Into::into)
    }
    /// Serializes to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("postcard::to_stdvec is infallible")
    }
}

/// Serializes to base32.
impl fmt::Display for PeerSource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let encoded = self.to_bytes();
        let mut text = data_encoding::BASE32_NOPAD.encode(&encoded);
        text.make_ascii_lowercase();
        write!(f, "{text}")
    }
}

/// Deserializes from base32.
impl FromStr for PeerSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = data_encoding::BASE32_NOPAD.decode(s.to_ascii_uppercase().as_bytes())?;
        let slf = Self::from_bytes(&bytes)?;
        Ok(slf)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Op {
    Put(SignedEntry),
}

#[derive(Debug)]
enum SyncState {
    Running,
    Finished,
    Failed(anyhow::Error),
}

#[derive(Debug)]
pub enum ToActor<S: store::Store> {
    SyncDoc {
        doc: Replica<S::Instance>,
        initial_peers: Vec<PeerSource>,
    },
    Shutdown,
}

/// Handle to a running live sync actor
#[derive(Debug, Clone)]
pub struct LiveSync<S: store::Store> {
    to_actor_tx: mpsc::Sender<ToActor<S>>,
    task: Shared<BoxFuture<'static, Result<(), Arc<JoinError>>>>,
}

impl<S: store::Store> LiveSync<S> {
    pub fn spawn(rt: Handle, endpoint: MagicEndpoint, gossip: Gossip) -> Self {
        let (to_actor_tx, to_actor_rx) = mpsc::channel(CHANNEL_CAP);
        let mut actor = Actor::new(endpoint, gossip, to_actor_rx);
        let task = rt.main().spawn(async move {
            if let Err(err) = actor.run().await {
                error!("live sync failed: {err:?}");
            }
        });
        let handle = LiveSync {
            to_actor_tx,
            task: task.map_err(Arc::new).boxed().shared(),
        };
        handle
    }

    /// Cancel the live sync.
    pub async fn cancel(&self) -> Result<()> {
        self.to_actor_tx.send(ToActor::<S>::Shutdown).await?;
        self.task.clone().await?;
        Ok(())
    }

    pub async fn add(
        &self,
        doc: Replica<S::Instance>,
        initial_peers: Vec<PeerSource>,
    ) -> Result<()> {
        self.to_actor_tx
            .send(ToActor::<S>::SyncDoc { doc, initial_peers })
            .await?;
        Ok(())
    }
}

// TODO: Also add `handle_connection` to the replica and track incoming sync requests here too.
// Currently peers might double-sync in both directions.
struct Actor<S: store::Store> {
    endpoint: MagicEndpoint,
    gossip: Gossip,

    docs: HashMap<TopicId, Replica<S::Instance>>,
    subscription: BoxStream<'static, Result<(TopicId, Event)>>,
    sync_state: HashMap<(TopicId, PeerId), SyncState>,

    to_actor_rx: mpsc::Receiver<ToActor<S>>,
    insert_entry_tx: flume::Sender<(TopicId, SignedEntry)>,
    insert_entry_rx: flume::Receiver<(TopicId, SignedEntry)>,

    pending_syncs: FuturesUnordered<BoxFuture<'static, (TopicId, PeerId, Result<()>)>>,
    pending_joins: FuturesUnordered<BoxFuture<'static, (TopicId, Result<()>)>>,
}

impl<S: store::Store> Actor<S> {
    pub fn new(
        endpoint: MagicEndpoint,
        gossip: Gossip,
        to_actor_rx: mpsc::Receiver<ToActor<S>>,
    ) -> Self {
        let (insert_tx, insert_rx) = flume::bounded(64);
        let sub = gossip.clone().subscribe_all().boxed();

        Self {
            gossip,
            endpoint,
            insert_entry_rx: insert_rx,
            insert_entry_tx: insert_tx,
            to_actor_rx,
            sync_state: Default::default(),
            pending_syncs: Default::default(),
            pending_joins: Default::default(),
            docs: Default::default(),
            subscription: sub,
        }
    }

    async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                biased;
                msg = self.to_actor_rx.recv() => {
                    match msg {
                        // received shutdown signal, or livesync handle was dropped:
                        // break loop and exit
                        Some(ToActor::Shutdown) | None => {
                            self.on_shutdown().await?;
                            break;
                        }
                        Some(ToActor::SyncDoc { doc, initial_peers }) => self.insert_doc(doc, initial_peers).await?,
                    }
                }
                // new gossip message
                Some(event) = self.subscription.next() => {
                    let (topic, event) = event?;
                    if let Err(err) = self.on_gossip_event(topic, event) {
                        error!("Failed to process gossip event: {err:?}");
                    }
                },
                entry = self.insert_entry_rx.recv_async() => {
                    let (topic, entry) = entry?;
                    self.on_insert_entry(topic, entry).await?;
                }
                Some((topic, peer, res)) = self.pending_syncs.next() => {
                    // let (topic, peer, res) = res.context("task sync_with_peer paniced")?;
                    self.on_sync_finished(topic, peer, res);

                }
                Some((topic, res)) = self.pending_joins.next() => {
                    if let Err(err) = res {
                        error!("failed to join {topic:?}: {err:?}");
                    }
                    // TODO: maintain some join state
                }
            }
        }
        Ok(())
    }

    fn sync_with_peer(&mut self, topic: TopicId, peer: PeerId) {
        let Some(doc) = self.docs.get(&topic) else {
            return;
        };
        // Check if we synced and only start sync if not yet synced
        // sync_with_peer is triggered on NeighborUp events, so might trigger repeatedly for the
        // same peers.
        // TODO: Track finished time and potentially re-run sync
        if let Some(_state) = self.sync_state.get(&(topic, peer)) {
            return;
        };
        // TODO: fixme (doc_id, peer)
        self.sync_state.insert((topic, peer), SyncState::Running);
        let task = {
            let endpoint = self.endpoint.clone();
            let doc = doc.clone();
            async move {
                debug!("sync with {peer}");
                // TODO: Make sure that the peer is dialable.
                let res = connect_and_sync::<S>(&endpoint, &doc, peer, None, &[]).await;
                debug!("> synced with {peer}: {res:?}");
                // collect metrics
                match &res {
                    Ok(_) => inc!(Metrics, initial_sync_success),
                    Err(_) => inc!(Metrics, initial_sync_failed),
                }
                (topic, peer, res)
            }
            .boxed()
        };
        self.pending_syncs.push(task);
    }

    async fn on_shutdown(&mut self) -> anyhow::Result<()> {
        for (topic, _doc) in self.docs.drain() {
            // TODO: Remove the on_insert callbacks
            self.gossip.quit(topic).await?;
        }
        Ok(())
    }

    async fn insert_doc(
        &mut self,
        doc: Replica<S::Instance>,
        initial_peers: Vec<PeerSource>,
    ) -> Result<()> {
        let peer_ids: Vec<PeerId> = initial_peers.iter().map(|p| p.peer_id).collect();

        // add addresses of initial peers to our endpoint address book
        for peer in &initial_peers {
            self.endpoint
                .add_known_addrs(peer.peer_id, peer.derp_region, &peer.addrs)
                .await?;
        }

        // join gossip for the topic to receive and send message
        let topic = TopicId::from_bytes(*doc.namespace().as_bytes());
        self.pending_joins.push({
            let peer_ids = peer_ids.clone();
            let gossip = self.gossip.clone();
            async move {
                match gossip.join(topic, peer_ids).await {
                    Err(err) => (topic, Err(err)),
                    Ok(fut) => (topic, fut.await),
                }
            }
            .boxed()
        });

        // setup replica insert notifications.
        let insert_entry_tx = self.insert_entry_tx.clone();
        doc.on_insert(Box::new(move |origin, entry| {
            // only care for local inserts, otherwise we'd do endless gossip loops
            if let InsertOrigin::Local = origin {
                // TODO: this is potentially blocking inside an async call. figure out a better solution
                insert_entry_tx.send((topic, entry)).ok();
            }
        }));
        self.docs.insert(topic, doc);
        // add addresses of initial peers to our endpoint address book
        for peer in &initial_peers {
            self.endpoint
                .add_known_addrs(peer.peer_id, peer.derp_region, &peer.addrs)
                .await?;
        }

        // trigger initial sync with initial peers
        for peer in peer_ids {
            self.sync_with_peer(topic, peer);
        }
        Ok(())
    }

    fn on_sync_finished(&mut self, topic: TopicId, peer: PeerId, res: Result<()>) {
        let state = match res {
            Ok(_) => SyncState::Finished,
            Err(err) => SyncState::Failed(err),
        };
        self.sync_state.insert((topic, peer), state);
    }

    fn on_gossip_event(&mut self, topic: TopicId, event: Event) -> Result<()> {
        let Some(doc) = self.docs.get(&topic) else {
            return Err(anyhow!("Missing doc for {topic:?}"));
        };
        match event {
            // We received a gossip message. Try to insert it into our replica.
            Event::Received(data, prev_peer) => {
                let op: Op = postcard::from_bytes(&data)?;
                match op {
                    Op::Put(entry) => doc.insert_remote_entry(entry, Some(prev_peer.to_bytes()))?,
                }
            }
            // A new neighbor appeared in the gossip swarm. Try to sync with it directly.
            // [Self::sync_with_peer] will check to not resync with peers synced previously in the
            // same session. TODO: Maybe this is too broad and leads to too many sync requests.
            Event::NeighborUp(peer) => self.sync_with_peer(topic, peer),
            _ => {}
        }
        Ok(())
    }

    /// A new entry was inserted locally. Broadcast a gossip message.
    async fn on_insert_entry(&mut self, topic: TopicId, entry: SignedEntry) -> Result<()> {
        let op = Op::Put(entry);
        let message = postcard::to_stdvec(&op)?.into();
        self.gossip.broadcast(topic, message).await?;
        Ok(())
    }
}
