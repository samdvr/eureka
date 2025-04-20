use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use chrono::Utc;
use etcd_client::{Client, LockOptions, WatchOptions};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::SearchEngineConfig;

#[derive(Error, Debug)]
pub enum ConsensusError {
    #[error("Etcd client error: {0}")]
    EtcdError(#[from] etcd_client::Error),

    #[error("Lock acquisition failed: {0}")]
    LockError(String),

    #[error("Watch error: {0}")]
    WatchError(String),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Timeout error: {0}")]
    TimeoutError(String),
}

type Result<T> = std::result::Result<T, ConsensusError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub address: String,
    pub is_leader: bool,
    pub last_heartbeat: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterState {
    pub leader_id: Option<String>,
    pub nodes: Vec<NodeInfo>,
}

pub struct Consensus {
    client: Arc<Mutex<Client>>,
    node_id: String,
    address: String,
    lock: Arc<RwLock<Option<etcd_client::LockResponse>>>,
    state: Arc<RwLock<ClusterState>>,
    config: SearchEngineConfig,
    shutdown_signal: Arc<AtomicBool>,
}

impl Consensus {
    pub async fn new(config: SearchEngineConfig, address: String) -> Result<Self> {
        let client = Client::connect(config.etcd_endpoints.clone(), None).await?;
        let node_id = Uuid::new_v4().to_string();

        let state = ClusterState {
            leader_id: None,
            nodes: Vec::new(),
        };

        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            node_id,
            address,
            lock: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(state)),
            config,
            shutdown_signal: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn start_election(&self) -> Result<()> {
        let lock_key = b"/eureka/leader";
        let mut lock_options = LockOptions::new();
        lock_options = lock_options.with_lease(self.config.lock_ttl_secs as i64);

        // Start election process
        self.election_loop(lock_key, lock_options.clone()).await?;

        // Start leader lease renewal in background
        self.register_node(true).await?;

        Ok(())
    }

    async fn election_loop(&self, lock_key: &[u8], lock_options: LockOptions) -> Result<()> {
        loop {
            if self.shutdown_signal.load(Ordering::SeqCst) {
                return Ok(());
            }

            // Use timeout to prevent deadlock
            let client_lock_result = timeout(
                Duration::from_secs(5),
                self.client
                    .lock()
                    .await
                    .lock(lock_key, Some(lock_options.clone())),
            )
            .await;

            match client_lock_result {
                Ok(Ok(lock)) => {
                    info!("Node {} acquired leadership", self.node_id);
                    let mut lock_guard = self.lock.write().await;
                    *lock_guard = Some(lock);

                    // Update cluster state
                    let mut state = self.state.write().await;
                    state.leader_id = Some(self.node_id.clone());

                    break;
                }
                Ok(Err(e)) => {
                    warn!("Failed to acquire leadership: {}", e);
                    tokio::time::sleep(Duration::from_secs(self.config.retry_interval_secs)).await;
                }
                Err(_) => {
                    warn!("Timeout while trying to acquire leadership lock");
                    // Reconnect etcd client on timeout
                    self.reconnect_client().await?;
                    tokio::time::sleep(Duration::from_secs(self.config.retry_interval_secs)).await;
                }
            }
        }

        Ok(())
    }

    async fn reconnect_client(&self) -> Result<()> {
        info!("Reconnecting to etcd cluster");
        match Client::connect(self.config.etcd_endpoints.clone(), None).await {
            Ok(new_client) => {
                // Replace client safely
                let mut client_guard = self.client.lock().await;
                *client_guard = new_client;
                Ok(())
            }
            Err(e) => {
                error!("Failed to reconnect to etcd: {}", e);
                Err(ConsensusError::EtcdError(e))
            }
        }
    }

    async fn register_node(&self, is_leader: bool) -> Result<()> {
        let heartbeat_key = format!("/eureka/nodes/{}", self.node_id);
        let node_info = NodeInfo {
            id: self.node_id.clone(),
            address: self.address.clone(),
            is_leader,
            last_heartbeat: Utc::now().timestamp(),
        };

        // Create a lease for heartbeats with the node data
        let value = serde_json::to_string(&node_info)?;
        let client_result = timeout(Duration::from_secs(5), self.client.lock())
            .await
            .map_err(|_| ConsensusError::TimeoutError("Timeout getting client lock".to_string()))?;

        let mut client = client_result;

        // Grant a lease with TTL
        let lease_id = client
            .lease_grant(self.config.heartbeat_interval_secs as i64 * 3, None)
            .await?
            .id();

        debug!(
            "Created lease {} with TTL {} seconds",
            lease_id,
            self.config.heartbeat_interval_secs * 3
        );

        // Associate the node info with the lease using a PutOptions
        let mut put_options = etcd_client::PutOptions::new();
        put_options = put_options.with_lease(lease_id);

        client
            .put(
                heartbeat_key.as_bytes(),
                value.as_bytes(),
                Some(put_options),
            )
            .await?;

        // Start periodic heartbeat to update timestamp and keep lease alive
        let client = self.client.clone();
        let heartbeat_key = heartbeat_key.clone();
        let interval = self.config.heartbeat_interval_secs;
        let shutdown_signal = self.shutdown_signal.clone();

        tokio::spawn(async move {
            let mut updated_node_info = node_info.clone();

            while !shutdown_signal.load(Ordering::SeqCst) {
                // Update timestamp for each heartbeat
                updated_node_info.last_heartbeat = Utc::now().timestamp();

                if let Ok(value) = serde_json::to_string(&updated_node_info) {
                    let client_result = timeout(Duration::from_secs(5), client.lock()).await;

                    if let Ok(mut client_guard) = client_result {
                        // Create put options with lease ID for each update
                        let mut put_options = etcd_client::PutOptions::new();
                        put_options = put_options.with_lease(lease_id);

                        // Writing with the same lease automatically keeps it alive
                        if let Err(e) = client_guard
                            .put(
                                heartbeat_key.as_bytes(),
                                value.as_bytes(),
                                Some(put_options),
                            )
                            .await
                        {
                            error!("Failed to send heartbeat: {}", e);
                        } else {
                            debug!("Successfully renewed node data and lease {}", lease_id);
                        }
                    } else {
                        error!("Timeout acquiring client lock for heartbeat");
                    }
                }
                tokio::time::sleep(Duration::from_secs(interval / 2)).await;
            }
        });

        Ok(())
    }

    pub async fn watch_cluster_state(&self) -> Result<()> {
        let watch_key = b"/eureka/nodes/";

        // Setup initial watch stream
        self.setup_watch_stream(watch_key).await?;

        Ok(())
    }

    async fn setup_watch_stream(&self, watch_key: &[u8]) -> Result<()> {
        let client_result = timeout(Duration::from_secs(5), self.client.lock())
            .await
            .map_err(|_| ConsensusError::TimeoutError("Timeout getting client lock".to_string()))?;

        let mut client = client_result;
        let (_, mut watch_stream) = client
            .watch(watch_key, Some(WatchOptions::new().with_prefix()))
            .await?;

        let state = self.state.clone();
        let node_id = self.node_id.clone();
        let shutdown_signal = self.shutdown_signal.clone();
        let client = self.client.clone();
        let leader_key = b"/eureka/leader";
        let self_state = self.state.clone();
        let self_lock = self.lock.clone();
        let self_node_id = self.node_id.clone();
        let config = self.config.clone();
        let watch_key = watch_key.to_vec();

        tokio::spawn(async move {
            let mut backoff_secs = 1;
            let max_backoff_secs = 60; // Maximum backoff of 1 minute

            'outer: loop {
                if shutdown_signal.load(Ordering::SeqCst) {
                    break;
                }

                while let Some(response) = watch_stream.next().await {
                    if shutdown_signal.load(Ordering::SeqCst) {
                        break 'outer;
                    }

                    match response {
                        Ok(watch_response) => {
                            // Reset backoff on successful response
                            backoff_secs = 1;

                            let events = watch_response.events();

                            // Get current state to update incrementally
                            let mut current_state = state.read().await.clone();
                            let mut state_changed = false;

                            for event in events {
                                if let Some(kv) = event.kv() {
                                    match event.event_type() {
                                        etcd_client::EventType::Put => {
                                            if let Ok(node_info) =
                                                serde_json::from_slice::<NodeInfo>(kv.value())
                                            {
                                                // Update or add node
                                                if let Some(existing) = current_state
                                                    .nodes
                                                    .iter_mut()
                                                    .find(|n| n.id == node_info.id)
                                                {
                                                    *existing = node_info;
                                                } else {
                                                    current_state.nodes.push(node_info);
                                                }
                                                state_changed = true;
                                            }
                                        }
                                        etcd_client::EventType::Delete => {
                                            // Extract node ID from the key
                                            let key = std::str::from_utf8(kv.key()).unwrap_or("");
                                            if let Some(id) = key.strip_prefix("/eureka/nodes/") {
                                                current_state.nodes.retain(|n| n.id != id);
                                                state_changed = true;

                                                // If leader was removed, try to become leader
                                                if current_state.leader_id.as_deref() == Some(id) {
                                                    info!("Leader node {} disappeared, attempting to acquire leadership", id);
                                                    current_state.leader_id = None;

                                                    tokio::spawn({
                                                        let client = client.clone();
                                                        let self_lock = self_lock.clone();
                                                        let self_state = self_state.clone();
                                                        let self_node_id = self_node_id.clone();
                                                        let config = config.clone();

                                                        async move {
                                                            let mut lock_options =
                                                                LockOptions::new();
                                                            lock_options = lock_options.with_lease(
                                                                config.lock_ttl_secs as i64,
                                                            );

                                                            // Try to acquire leadership
                                                            let client_result = timeout(
                                                                Duration::from_secs(5),
                                                                client.lock(),
                                                            )
                                                            .await;

                                                            if let Ok(mut client_guard) =
                                                                client_result
                                                            {
                                                                if let Ok(lock) = client_guard
                                                                    .lock(
                                                                        leader_key,
                                                                        Some(lock_options),
                                                                    )
                                                                    .await
                                                                {
                                                                    info!("Node {} acquired leadership after failover", self_node_id);
                                                                    let mut lock_guard =
                                                                        self_lock.write().await;
                                                                    *lock_guard = Some(lock);

                                                                    // Update state
                                                                    let mut state =
                                                                        self_state.write().await;
                                                                    state.leader_id =
                                                                        Some(self_node_id.clone());
                                                                }
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Only update if changes occurred
                            if state_changed {
                                // Update leader if not already set
                                if current_state.leader_id.is_none() {
                                    if let Some(leader) =
                                        current_state.nodes.iter().find(|n| n.is_leader)
                                    {
                                        current_state.leader_id = Some(leader.id.clone());
                                    }
                                }

                                // Check for stale nodes (heartbeat older than 3x interval)
                                let now = Utc::now().timestamp();
                                let max_heartbeat_age = 3 * config.heartbeat_interval_secs as i64;
                                current_state.nodes.retain(|node| {
                                    (now - node.last_heartbeat) <= max_heartbeat_age
                                });

                                // Write updated state
                                let mut state_write = state.write().await;
                                *state_write = current_state.clone();

                                debug!("Cluster state updated: {:?}", state_write);
                            }
                        }
                        Err(e) => {
                            error!("Watch error: {}", e);
                            // Break from inner loop to trigger reconnection
                            break;
                        }
                    }
                }

                // Watch stream ended or error occurred, attempt to reconnect
                info!(
                    "Watch stream ended, attempting to reconnect in {} seconds",
                    backoff_secs
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;

                // Exponential backoff with jitter
                let jitter = rand::random::<u64>() % 1000;
                backoff_secs = std::cmp::min(backoff_secs * 2 + jitter / 1000, max_backoff_secs);

                // Try to create a new watch stream
                match Self::reconnect_watch_stream(&client, &watch_key).await {
                    Ok(new_stream) => {
                        info!("Successfully reconnected watch stream");
                        watch_stream = new_stream;
                    }
                    Err(e) => {
                        error!("Failed to reconnect watch stream: {}", e);
                        // Continue to outer loop which will retry after backoff
                    }
                }
            }

            info!("Watch stream monitor ended");
        });

        Ok(())
    }

    async fn reconnect_watch_stream(
        client: &Arc<Mutex<Client>>,
        watch_key: &[u8],
    ) -> Result<etcd_client::WatchStream> {
        let max_attempts = 3;
        let mut attempt = 0;

        while attempt < max_attempts {
            attempt += 1;

            match timeout(Duration::from_secs(5), client.lock()).await {
                Ok(mut client_guard) => {
                    match client_guard
                        .watch(watch_key, Some(WatchOptions::new().with_prefix()))
                        .await
                    {
                        Ok((_, stream)) => {
                            return Ok(stream);
                        }
                        Err(e) => {
                            error!(
                                "Watch reconnection attempt {}/{} failed: {}",
                                attempt, max_attempts, e
                            );

                            if attempt < max_attempts {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            } else {
                                return Err(ConsensusError::WatchError(format!(
                                    "Failed to reconnect watch after {} attempts: {}",
                                    max_attempts, e
                                )));
                            }
                        }
                    }
                }
                Err(_) => {
                    error!(
                        "Timeout getting client lock for watch reconnection attempt {}/{}",
                        attempt, max_attempts
                    );

                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    } else {
                        return Err(ConsensusError::TimeoutError(format!(
                            "Timeout getting client lock after {} attempts",
                            max_attempts
                        )));
                    }
                }
            }
        }

        // Should never reach here due to the return statements in the loop
        unreachable!()
    }

    pub async fn is_leader(&self) -> bool {
        let state = self.state.read().await;
        state.leader_id.as_ref() == Some(&self.node_id)
    }

    pub async fn get_cluster_state(&self) -> ClusterState {
        self.state.read().await.clone()
    }

    pub async fn shutdown(&self) -> Result<()> {
        // Signal all background tasks to stop
        self.shutdown_signal.store(true, Ordering::SeqCst);

        // Release leadership if we have it
        if self.is_leader().await {
            let lock_guard = self.lock.read().await;
            if let Some(lock) = &*lock_guard {
                let client_result = timeout(Duration::from_secs(5), self.client.lock()).await;

                if let Ok(mut client) = client_result {
                    // Unlock the leadership lock
                    let key = lock.key();
                    if let Err(e) = client.unlock(key).await {
                        error!("Failed to release leadership lock: {}", e);
                    }
                }
            }
        }

        // Remove our node info
        let heartbeat_key = format!("/eureka/nodes/{}", self.node_id);
        let client_result = timeout(Duration::from_secs(5), self.client.lock()).await;

        if let Ok(mut client) = client_result {
            if let Err(e) = client.delete(heartbeat_key, None).await {
                error!("Failed to remove node from cluster: {}", e);
            }
        }

        // Give time for background tasks to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(())
    }
}

impl Drop for Consensus {
    fn drop(&mut self) {
        // Release the lock if we have it
        if let Some(lock) = self.lock.try_write().ok().and_then(|mut g| g.take()) {
            let client = self.client.clone();
            let lock_key = b"/eureka/leader";
            let shutdown_signal = self.shutdown_signal.clone();

            // Signal shutdown
            shutdown_signal.store(true, Ordering::SeqCst);

            tokio::spawn(async move {
                let client_result = timeout(Duration::from_secs(2), client.lock()).await;
                if let Ok(mut client) = client_result {
                    if let Err(e) = client.unlock(lock.key()).await {
                        error!("Failed to release leadership lock: {}", e);
                    }
                }
            });
        }
    }
}
