//! Implementation of the iroh-sync protocol

use std::net::SocketAddr;

use anyhow::{bail, ensure, Context, Result};
use bytes::BytesMut;
use iroh_net::{magic_endpoint::get_peer_id, tls::PeerId, MagicEndpoint};
use iroh_sync::{
    store,
    sync::{NamespaceId, Replica},
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::debug;

/// The ALPN identifier for the iroh-sync protocol
pub const SYNC_ALPN: &[u8] = b"/iroh-sync/1";

mod content;
mod live;
pub mod metrics;
pub mod node;

pub use content::*;
pub use live::*;

/// Sync Protocol
///
/// - Init message: signals which namespace is being synced
/// - N Sync messages
///
/// On any error and on success the substream is closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Message {
    Init {
        /// Namespace to sync
        namespace: NamespaceId,
        /// Initial message
        message: iroh_sync::sync::ProtocolMessage,
    },
    Sync(iroh_sync::sync::ProtocolMessage),
}

/// Connect to a peer and sync a replica
pub async fn connect_and_sync<S: store::Store>(
    endpoint: &MagicEndpoint,
    doc: &Replica<S::Instance>,
    peer_id: PeerId,
    derp_region: Option<u16>,
    addrs: &[SocketAddr],
) -> anyhow::Result<()> {
    debug!("sync with peer {}: start", peer_id);
    let connection = endpoint
        .connect(peer_id, SYNC_ALPN, derp_region, addrs)
        .await
        .context("dial_and_sync")?;
    let (mut send_stream, mut recv_stream) = connection.open_bi().await?;
    let res = run_alice::<S, _, _>(&mut send_stream, &mut recv_stream, doc, Some(peer_id)).await;
    debug!("sync with peer {}: finish {:?}", peer_id, res);
    res
}

/// Runs the initiator side of the sync protocol.
pub async fn run_alice<S: store::Store, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    writer: &mut W,
    reader: &mut R,
    alice: &Replica<S::Instance>,
    peer: Option<PeerId>,
) -> Result<()> {
    let peer = peer.map(|peer| peer.to_bytes());
    let mut buffer = BytesMut::with_capacity(1024);

    // Init message

    let init_message = Message::Init {
        namespace: alice.namespace(),
        message: alice.sync_initial_message().map_err(Into::into)?,
    };
    let msg_bytes = postcard::to_stdvec(&init_message)?;
    iroh_bytes::protocol::write_lp(writer, &msg_bytes).await?;

    // Sync message loop

    while let Some(read) = iroh_bytes::protocol::read_lp(&mut *reader, &mut buffer).await? {
        debug!("read {}", read.len());
        let msg = postcard::from_bytes(&read)?;
        match msg {
            Message::Init { .. } => {
                bail!("unexpected message: init");
            }
            Message::Sync(msg) => {
                if let Some(msg) = alice.sync_process_message(msg, peer).map_err(Into::into)? {
                    send_sync_message(writer, msg).await?;
                } else {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Handle an iroh-sync connection and sync all shared documents in the replica store.
pub async fn handle_connection<S: store::Store>(
    connecting: quinn::Connecting,
    replica_store: S,
) -> Result<()> {
    let connection = connecting.await?;
    debug!("> connection established!");
    let peer_id = get_peer_id(&connection).await?;
    let (mut send_stream, mut recv_stream) = connection.accept_bi().await?;

    run_bob(
        &mut send_stream,
        &mut recv_stream,
        replica_store,
        Some(peer_id),
    )
    .await?;
    send_stream.finish().await?;

    debug!("done");

    Ok(())
}

/// Runs the receiver side of the sync protocol.
pub async fn run_bob<S: store::Store, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    writer: &mut W,
    reader: &mut R,
    replica_store: S,
    peer: Option<PeerId>,
) -> Result<()> {
    let peer = peer.map(|peer| peer.to_bytes());
    let mut buffer = BytesMut::with_capacity(1024);

    let mut replica = None;
    while let Some(read) = iroh_bytes::protocol::read_lp(&mut *reader, &mut buffer).await? {
        debug!("read {}", read.len());
        let msg = postcard::from_bytes(&read)?;

        match msg {
            Message::Init { namespace, message } => {
                ensure!(replica.is_none(), "double init message");

                match replica_store.get_replica(&namespace)? {
                    Some(r) => {
                        debug!("starting sync for {}", namespace);
                        if let Some(msg) =
                            r.sync_process_message(message, peer).map_err(Into::into)?
                        {
                            send_sync_message(writer, msg).await?;
                        } else {
                            break;
                        }
                        replica = Some(r);
                    }
                    None => {
                        // TODO: this should be possible.
                        bail!("unable to synchronize unknown namespace: {}", namespace);
                    }
                }
            }
            Message::Sync(msg) => match replica {
                Some(ref replica) => {
                    if let Some(msg) = replica
                        .sync_process_message(msg, peer)
                        .map_err(Into::into)?
                    {
                        send_sync_message(writer, msg).await?;
                    } else {
                        break;
                    }
                }
                None => {
                    bail!("unexpected sync message without init");
                }
            },
        }
    }

    Ok(())
}

async fn send_sync_message<W: AsyncWrite + Unpin>(
    stream: &mut W,
    msg: iroh_sync::sync::ProtocolMessage,
) -> Result<()> {
    let msg_bytes = postcard::to_stdvec(&Message::Sync(msg))?;
    iroh_bytes::protocol::write_lp(stream, &msg_bytes).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use iroh_sync::{store::Store as _, sync::Namespace};

    use super::*;

    #[tokio::test]
    async fn test_sync_simple() -> Result<()> {
        let mut rng = rand::thread_rng();

        let alice_replica_store = store::memory::Store::default();
        // For now uses same author on both sides.
        let author = alice_replica_store.new_author(&mut rng).unwrap();

        let namespace = Namespace::new(&mut rng);

        let alice_replica = alice_replica_store.new_replica(namespace.clone()).unwrap();
        alice_replica
            .hash_and_insert("hello bob", &author, "from alice")
            .unwrap();

        let bob_replica_store = store::memory::Store::default();
        let bob_replica = bob_replica_store.new_replica(namespace.clone()).unwrap();
        bob_replica
            .hash_and_insert("hello alice", &author, "from bob")
            .unwrap();

        assert_eq!(
            bob_replica_store
                .get_all(bob_replica.namespace())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            alice_replica_store
                .get_all(alice_replica.namespace())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );

        let (alice, bob) = tokio::io::duplex(64);

        let (mut alice_reader, mut alice_writer) = tokio::io::split(alice);
        let replica = alice_replica.clone();
        let alice_task = tokio::task::spawn(async move {
            run_alice::<store::memory::Store, _, _>(
                &mut alice_writer,
                &mut alice_reader,
                &replica,
                None,
            )
            .await
        });

        let (mut bob_reader, mut bob_writer) = tokio::io::split(bob);
        let bob_replica_store_task = bob_replica_store.clone();
        let bob_task = tokio::task::spawn(async move {
            run_bob::<store::memory::Store, _, _>(
                &mut bob_writer,
                &mut bob_reader,
                bob_replica_store_task,
                None,
            )
            .await
        });

        alice_task.await??;
        bob_task.await??;

        assert_eq!(
            bob_replica_store
                .get_all(bob_replica.namespace())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            alice_replica_store
                .get_all(alice_replica.namespace())
                .unwrap()
                .collect::<Result<Vec<_>>>()
                .unwrap()
                .len(),
            2
        );

        Ok(())
    }
}
