pub mod api;
pub mod consensus;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::StreamExt;
use object_store::{path::Path as ObjectPath, ObjectStore};
use serde::{Deserialize, Serialize};
use tantivy::doc;
use tantivy::schema::{Schema, Value, STORED, TEXT};
use tantivy::{Index, ReloadPolicy};
use tempfile::TempDir;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::consensus::{Consensus, ConsensusError};

#[derive(Error, Debug)]
pub enum SearchEngineError {
    #[error("Index error: {0}")]
    IndexError(#[from] tantivy::TantivyError),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Object store error: {0}")]
    ObjectStoreError(#[from] object_store::Error),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Field not found: {0}")]
    FieldNotFound(String),

    #[error("Query parsing error: {0}")]
    QueryParseError(#[from] tantivy::query::QueryParserError),

    #[error("Consensus error: {0}")]
    ConsensusError(#[from] ConsensusError),

    #[error("Task join error: {0}")]
    JoinError(String),
}

pub type Result<T> = std::result::Result<T, SearchEngineError>;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SearchEngineConfig {
    #[serde(default = "default_index_size")]
    pub index_buffer_size: usize,
    pub storage_path: String,
    pub index_prefix: String,
    #[serde(default = "default_max_results")]
    pub max_search_results: usize,
    #[serde(default = "default_etcd_endpoints")]
    pub etcd_endpoints: Vec<String>,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_lock_ttl")]
    pub lock_ttl_secs: u64,
    #[serde(default = "default_retry_interval")]
    pub retry_interval_secs: u64,
    #[serde(default = "default_aws_region")]
    pub aws_region: Option<String>,
    #[serde(default = "default_aws_access_key_id")]
    pub aws_access_key_id: Option<String>,
    #[serde(default = "default_aws_secret_access_key")]
    pub aws_secret_access_key: Option<String>,
    #[serde(default = "default_tls_enabled")]
    pub tls_enabled: bool,
    #[serde(default = "default_tls_cert_path")]
    pub tls_cert_path: Option<String>,
    #[serde(default = "default_tls_key_path")]
    pub tls_key_path: Option<String>,
    #[serde(default = "default_auth_enabled")]
    pub auth_enabled: bool,
    #[serde(default = "default_auth_token")]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub scatter_gather_enabled: bool,
}

impl Default for SearchEngineConfig {
    fn default() -> Self {
        Self {
            index_buffer_size: default_index_size(),
            storage_path: "object_store".to_string(),
            index_prefix: "index_data".to_string(),
            max_search_results: default_max_results(),
            etcd_endpoints: default_etcd_endpoints(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            lock_ttl_secs: default_lock_ttl(),
            retry_interval_secs: default_retry_interval(),
            aws_region: default_aws_region(),
            aws_access_key_id: default_aws_access_key_id(),
            aws_secret_access_key: default_aws_secret_access_key(),
            tls_enabled: default_tls_enabled(),
            tls_cert_path: default_tls_cert_path(),
            tls_key_path: default_tls_key_path(),
            auth_enabled: default_auth_enabled(),
            auth_token: default_auth_token(),
            scatter_gather_enabled: false,
        }
    }
}

fn default_index_size() -> usize {
    50_000_000 // 50MB
}

fn default_max_results() -> usize {
    100
}

fn default_etcd_endpoints() -> Vec<String> {
    vec!["http://localhost:2379".to_string()]
}

fn default_heartbeat_interval() -> u64 {
    5
}

fn default_lock_ttl() -> u64 {
    30
}

fn default_retry_interval() -> u64 {
    1
}

fn default_aws_region() -> Option<String> {
    None
}

fn default_aws_access_key_id() -> Option<String> {
    None
}

fn default_aws_secret_access_key() -> Option<String> {
    None
}

fn default_tls_enabled() -> bool {
    false
}

fn default_tls_cert_path() -> Option<String> {
    None
}

fn default_tls_key_path() -> Option<String> {
    None
}

fn default_auth_enabled() -> bool {
    false
}

fn default_auth_token() -> Option<String> {
    None
}

pub struct SearchEngine {
    pub index: Index,
    object_store: Arc<dyn ObjectStore>,
    index_dir: TempDir,
    pub config: SearchEngineConfig,
    pub consensus: Arc<Consensus>,
}

impl SearchEngine {
    pub async fn new(
        object_store: Arc<dyn ObjectStore>,
        schema: Schema,
        config: SearchEngineConfig,
        address: String,
    ) -> Result<Self> {
        let index_dir = tempfile::tempdir()?;
        debug!(
            "Created temporary directory for index at {:?}",
            index_dir.path()
        );

        let index = Index::create_in_dir(index_dir.path(), schema)?;
        debug!("Created tantivy index");

        // Initialize consensus
        let consensus = Consensus::new(config.clone(), address).await?;
        let consensus = Arc::new(consensus);

        // Start election and watch cluster state
        consensus.start_election().await?;
        consensus.watch_cluster_state().await?;

        Ok(Self {
            index,
            object_store,
            index_dir,
            config,
            consensus,
        })
    }

    pub async fn add_documents(&self, documents: Vec<tantivy::TantivyDocument>) -> Result<()> {
        // Only allow indexing on the leader
        if !self.consensus.is_leader().await {
            return Err(SearchEngineError::ConsensusError(
                ConsensusError::LockError("Not the leader node".to_string()),
            ));
        }

        let start = Instant::now();
        let mut writer = self.index.writer(self.config.index_buffer_size)?;

        let doc_count = documents.len();
        info!("Adding {} documents to the index", doc_count);

        for doc in documents {
            writer.add_document(doc)?;
        }

        info!("Committing index changes");
        writer.commit()?;

        info!("Indexed {} documents in {:?}", doc_count, start.elapsed());
        Ok(())
    }

    pub fn search(&self, query_str: &str) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        if self.config.scatter_gather_enabled {
            // We can't handle scatter_gather in a sync context
            return Err(SearchEngineError::ConfigError(
                "Scatter-gather search is not supported in sync context".to_string(),
            ));
        }

        let index = self.index.clone();
        let config = self.config.clone();
        let query_str = query_str.to_string(); // Clone query_str to avoid lifetime issues

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let searcher = reader.searcher();
        let schema = index.schema();
        let title = schema.get_field("title").unwrap();
        let body = schema.get_field("body").unwrap();
        let query_parser = tantivy::query::QueryParser::for_index(&index, vec![title, body]);
        let query = query_parser.parse_query(&query_str)?;
        let top_docs = searcher.search(
            &query,
            &tantivy::collector::TopDocs::with_limit(config.max_search_results),
        )?;
        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved_doc = searcher.doc::<tantivy::TantivyDocument>(doc_address)?;
            results.push((score, retrieved_doc));
        }
        Ok(results)
    }

    pub async fn persist_to_store(&self) -> Result<()> {
        let index_path = self.index_dir.path();
        let start = Instant::now();
        let mut file_count = 0;

        info!(
            "Persisting index to object store with prefix: {}",
            self.config.index_prefix
        );

        for entry in fs::read_dir(index_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let file_name = path.file_name().unwrap().to_string_lossy().to_string();
                let object_path =
                    ObjectPath::from(format!("{}/{}", self.config.index_prefix, file_name));

                debug!("Reading file: {}", file_name);
                let content = fs::read(&path)?;

                debug!("Writing to object store: {}", object_path);
                self.object_store.put(&object_path, content.into()).await?;
                file_count += 1;
            }
        }

        info!(
            "Successfully persisted {} index files to object store in {:?}",
            file_count,
            start.elapsed()
        );

        Ok(())
    }

    pub async fn load_from_store(&self) -> Result<()> {
        let start = Instant::now();
        let prefix_path = ObjectPath::from(self.config.index_prefix.clone());
        let objects_stream = self.object_store.list(Some(&prefix_path));
        futures::pin_mut!(objects_stream);

        let index_path = self.index_dir.path();
        let mut file_count = 0;

        info!(
            "Loading index from object store with prefix: {}",
            self.config.index_prefix
        );

        while let Some(meta_result) = objects_stream.next().await {
            let meta = meta_result?;
            let path = meta.location;
            let file_name = Path::new(path.as_ref())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();

            let local_path = index_path.join(file_name);

            debug!("Fetching from object store: {}", path);
            let get_result = self.object_store.get(&path).await?;
            let bytes = get_result.bytes().await?;

            debug!("Writing to local file: {}", local_path.display());
            fs::write(local_path, bytes)?;
            file_count += 1;
        }

        if file_count == 0 {
            warn!(
                "No index files found in object store with prefix: {}",
                self.config.index_prefix
            );
        } else {
            info!(
                "Successfully loaded {} index files from object store in {:?}",
                file_count,
                start.elapsed()
            );
        }

        Ok(())
    }

    pub async fn search_async(
        &self,
        query_str: &str,
        force_distributed: bool,
    ) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        if force_distributed || self.config.scatter_gather_enabled {
            info!("Using scatter-gather search for query: {}", query_str);
            return self.scatter_gather_search(query_str).await;
        }

        // For non scatter-gather search, use the synchronous implementation in a blocking task
        let index = self.index.clone();
        let config = self.config.clone();
        let query = query_str.to_string();

        tokio::task::spawn_blocking(move || {
            let reader = index
                .reader_builder()
                .reload_policy(ReloadPolicy::Manual)
                .try_into()?;
            let searcher = reader.searcher();
            let schema = index.schema();
            let title = schema.get_field("title").unwrap();
            let body = schema.get_field("body").unwrap();
            let query_parser = tantivy::query::QueryParser::for_index(&index, vec![title, body]);
            let parsed_query = query_parser.parse_query(&query)?;
            let top_docs = searcher.search(
                &parsed_query,
                &tantivy::collector::TopDocs::with_limit(config.max_search_results),
            )?;
            let mut results = Vec::with_capacity(top_docs.len());
            for (score, doc_address) in top_docs {
                let retrieved_doc = searcher.doc::<tantivy::TantivyDocument>(doc_address)?;
                results.push((score, retrieved_doc));
            }
            Ok(results)
        })
        .await
        .map_err(|e| SearchEngineError::JoinError(e.to_string()))?
    }

    pub async fn scatter_gather_search(
        &self,
        query_str: &str,
    ) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        let client = reqwest::Client::new();
        let cluster_state = self.consensus.get_cluster_state().await;
        let nodes = cluster_state.nodes;
        let query_encoded = urlencoding::encode(query_str).to_string();

        let mut tasks = Vec::new();
        for node in nodes.into_iter() {
            let url = format!("http://{}/search?q={}", node.address, query_encoded);
            let client_clone = client.clone();
            let task = tokio::spawn(async move {
                let response = client_clone.get(&url).send().await;
                match response {
                    Ok(resp) => resp.json::<RemoteSearchResponse>().await,
                    Err(e) => Err(e),
                }
            });
            tasks.push(task);
        }

        let responses = futures::future::join_all(tasks).await;
        let mut aggregated: Vec<(f32, tantivy::TantivyDocument)> = Vec::new();

        let schema = self.index.schema();
        let title_field = schema.get_field("title").unwrap();
        let body_field = schema.get_field("body").unwrap();

        for res in responses {
            if let Ok(remote_response) = res {
                match remote_response {
                    Ok(response) => {
                        for remote in response.results {
                            let mut doc = tantivy::TantivyDocument::default();
                            doc.add_text(title_field, remote.title);
                            doc.add_text(body_field, remote.body);
                            aggregated.push((remote.score, doc));
                        }
                    }
                    Err(e) => {
                        eprintln!("Error in remote response: {:?}", e);
                    }
                }
            } else {
                eprintln!("Error in response: {:?}", res);
            }
        }
        aggregated.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        aggregated.truncate(self.config.max_search_results);
        Ok(aggregated)
    }

    pub fn print_search_results(&self, results: &[(f32, tantivy::TantivyDocument)], query: &str) {
        println!(
            "Search Results for '{}' ({} matches):",
            query,
            results.len()
        );
        if results.is_empty() {
            println!("No results found.");
            return;
        }
        let schema = self.index.schema();
        let title_field = schema.get_field("title").unwrap();
        let body_field = schema.get_field("body").unwrap();
        for (i, (score, doc)) in results.iter().enumerate() {
            println!("Result #{} (Score: {:.4}):", i + 1, score);
            if let Some(title_value) = doc.get_first(title_field) {
                if let Some(text) = title_value.as_str() {
                    println!("  Title: {}", text);
                } else {
                    println!("  Title: N/A");
                }
            }
            if let Some(body_value) = doc.get_first(body_field) {
                if let Some(text) = body_value.as_str() {
                    println!("  Body: {}", text);
                } else {
                    println!("  Body: N/A");
                }
            }
        }
    }
}

pub fn create_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    schema_builder.add_text_field("title", TEXT | STORED);
    schema_builder.add_text_field("body", TEXT | STORED);
    schema_builder.build()
}

#[derive(Debug, serde::Deserialize)]
struct RemoteSearchResponse {
    query: String,
    total: usize,
    results: Vec<RemoteSearchResult>,
}

#[derive(Debug, serde::Deserialize)]
struct RemoteSearchResult {
    score: f32,
    title: String,
    body: String,
}
