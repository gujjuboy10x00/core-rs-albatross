use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::task::Waker;

use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt};
use parking_lot::RwLock;

use nimiq_blockchain::Blockchain;
use nimiq_hash::Blake2bHash;
use nimiq_network_interface::network::{Network, SubscribeEvents};

use crate::messages::Checkpoint;
use crate::sync::history::cluster::{SyncCluster, SyncClusterResult};
use crate::sync::request_component::HistorySyncStream;

#[derive(Clone)]
pub(crate) struct EpochIds<T> {
    pub locator_found: bool,
    pub ids: Vec<Blake2bHash>,
    pub checkpoint: Option<Checkpoint>, // The most recent checkpoint block in the latest epoch.
    pub first_epoch_number: usize,
    pub sender: T,
}

impl<T> EpochIds<T> {
    #[inline]
    pub(crate) fn checkpoint_epoch_number(&self) -> usize {
        self.first_epoch_number + self.ids.len()
    }

    #[inline]
    pub(crate) fn last_epoch_number(&self) -> usize {
        self.checkpoint_epoch_number().saturating_sub(1)
    }
}

pub(crate) enum Job<TNetwork: Network> {
    PushBatchSet(usize, Blake2bHash, BoxFuture<'static, SyncClusterResult>),
    FinishCluster(SyncCluster<TNetwork>, SyncClusterResult),
}

pub struct HistorySync<TNetwork: Network> {
    pub(crate) blockchain: Arc<RwLock<Blockchain>>,
    pub(crate) network: Arc<TNetwork>,
    pub(crate) network_event_rx: SubscribeEvents<TNetwork::PeerId>,
    pub(crate) peers: HashMap<TNetwork::PeerId, usize>,
    pub(crate) epoch_ids_stream:
        FuturesUnordered<BoxFuture<'static, Option<EpochIds<TNetwork::PeerId>>>>,
    pub(crate) epoch_clusters: VecDeque<SyncCluster<TNetwork>>,
    pub(crate) checkpoint_clusters: VecDeque<SyncCluster<TNetwork>>,
    pub(crate) active_cluster: Option<SyncCluster<TNetwork>>,
    pub(crate) job_queue: VecDeque<Job<TNetwork>>,
    pub(crate) waker: Option<Waker>,
}

#[derive(Debug)]
pub enum HistorySyncReturn<T> {
    Good(T),
    Outdated(T),
}

impl<TNetwork: Network> HistorySync<TNetwork> {
    pub(crate) const MAX_CLUSTERS: usize = 100;
    pub(crate) const MAX_QUEUED_JOBS: usize = 4;

    pub fn new(
        blockchain: Arc<RwLock<Blockchain>>,
        network: Arc<TNetwork>,
        network_event_rx: SubscribeEvents<TNetwork::PeerId>,
    ) -> Self {
        Self {
            blockchain,
            network,
            network_event_rx,
            peers: HashMap::new(),
            epoch_ids_stream: FuturesUnordered::new(),
            epoch_clusters: VecDeque::new(),
            checkpoint_clusters: VecDeque::new(),
            active_cluster: None,
            job_queue: VecDeque::new(),
            waker: None,
        }
    }

    pub fn peers(&self) -> impl Iterator<Item = &TNetwork::PeerId> {
        self.peers.keys()
    }

    pub fn remove_peer(&mut self, peer_id: TNetwork::PeerId) {
        for cluster in self.epoch_clusters.iter_mut() {
            cluster.remove_peer(&peer_id);
        }
        for cluster in self.checkpoint_clusters.iter_mut() {
            cluster.remove_peer(&peer_id);
        }
        if let Some(cluster) = self.active_cluster.as_mut() {
            cluster.remove_peer(&peer_id);
        }
        for job in self.job_queue.iter_mut() {
            if let Job::FinishCluster(ref mut cluster, _) = job {
                cluster.remove_peer(&peer_id);
            }
        }
    }
}

impl<TNetwork: Network> HistorySyncStream<TNetwork::PeerId> for HistorySync<TNetwork> {
    fn add_peer(&self, peer_id: TNetwork::PeerId) {
        trace!("Requesting epoch ids for peer: {:?}", peer_id);
        let future = Self::request_epoch_ids(
            Arc::clone(&self.blockchain),
            Arc::clone(&self.network),
            peer_id,
        )
        .boxed();
        self.epoch_ids_stream.push(future);

        // Pushing the future to FuturesUnordered above does not wake the task that
        // polls `epoch_ids_stream`. Therefore, we need to wake the task manually.
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }
}
