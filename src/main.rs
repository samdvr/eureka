pub mod api;
pub mod consensus;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use futures::stream::StreamExt;
use object_store::local::LocalFileSystem;
use object_store::{path::Path as ObjectPath, ObjectStore};
use serde::{Deserialize, Serialize};
use tantivy::doc;
use tantivy::schema::{Schema, Value, STORED, TEXT};
use tantivy::{Index, ReloadPolicy};
use tempfile::TempDir;
use thiserror::Error;
use tokio::runtime::Runtime;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
}

type Result<T> = std::result::Result<T, SearchEngineError>;

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

#[derive(Parser)]
#[clap(version, about = "A search engine with object store persistence")]
struct CliArgs {
    #[clap(long, env = "CONFIG_PATH")]
    config_path: Option<String>,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Parser)]
enum Command {
    #[clap(about = "Index documents")]
    Index,

    #[clap(about = "Search for documents")]
    Search {
        #[clap(help = "The search query")]
        query: String,
    },

    #[clap(about = "Start the API server")]
    Serve {
        #[clap(long, env = "API_PORT", default_value = "3000")]
        port: u16,
    },
}

pub struct SearchEngine {
    index: Index,
    object_store: Arc<dyn ObjectStore>,
    index_dir: TempDir,
    config: SearchEngineConfig,
    consensus: Arc<Consensus>,
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

    pub async fn search_async(
        &self,
        query_str: &str,
        force_distributed: bool,
    ) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        // Simplified implementation - just use the sync search for now
        self.search(query_str)
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

    pub fn add_sample_documents(&self) -> Result<()> {
        let mut writer = self.index.writer(self.config.index_buffer_size)?;

        info!("Adding sample documents to the index");

        let schema = self.index.schema();
        let title = schema.get_field("title").unwrap();
        let body = schema.get_field("body").unwrap();

        // Add some documents
        writer.add_document(doc!(
            title => "The Lord of the Rings",
            body => "An epic fantasy novel by J.R.R. Tolkien"
        ))?;

        writer.add_document(doc!(
            title => "The Hobbit",
            body => "A fantasy novel by J.R.R. Tolkien"
        ))?;

        writer.add_document(doc!(
            title => "1984",
            body => "A dystopian novel by George Orwell"
        ))?;

        writer.add_document(doc!(
            title => "To Kill a Mockingbird",
            body => "A novel by Harper Lee about racial inequality"
        ))?;

        writer.add_document(doc!(
            title => "Pride and Prejudice",
            body => "A romantic novel by Jane Austen"
        ))?;

        info!("Committing sample documents");
        writer.commit()?;

        info!("Added 5 sample documents to the index");
        Ok(())
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

    pub fn search(&self, query_str: &str) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        let start = Instant::now();
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;

        let searcher = reader.searcher();

        // Create a query parser
        let schema = self.index.schema();
        let title = schema.get_field("title").unwrap();
        let body = schema.get_field("body").unwrap();

        let query_parser = tantivy::query::QueryParser::for_index(&self.index, vec![title, body]);

        // Parse the query
        debug!("Searching for: {}", query_str);
        let query = query_parser.parse_query(query_str)?;

        // Search with the query
        let top_docs = searcher.search(
            &query,
            &tantivy::collector::TopDocs::with_limit(self.config.max_search_results),
        )?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved_doc = searcher.doc(doc_address)?;
            results.push((score, retrieved_doc));
        }

        info!(
            "Found {} results for query '{}' in {:?}",
            results.len(),
            query_str,
            start.elapsed()
        );

        Ok(results)
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

            // Extract and print fields
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

fn create_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    schema_builder.add_text_field("title", TEXT | STORED);
    schema_builder.add_text_field("body", TEXT | STORED);
    schema_builder.build()
}

fn get_config(config_path: Option<String>) -> Result<SearchEngineConfig> {
    match config_path {
        Some(path) => {
            info!("Loading config from {}", path);
            let config_str = fs::read_to_string(&path).map_err(|e| {
                SearchEngineError::ConfigError(format!("Failed to read config file: {}", e))
            })?;

            serde_json::from_str(&config_str).map_err(|e| {
                SearchEngineError::ConfigError(format!("Failed to parse config: {}", e))
            })
        }
        None => {
            info!("Using default configuration");
            Ok(SearchEngineConfig::default())
        }
    }
}

fn setup_object_store(config: &SearchEngineConfig) -> Result<Arc<dyn ObjectStore>> {
    let store_path = Path::new(&config.storage_path);
    fs::create_dir_all(store_path)?;

    info!(
        "Using local file system object store at: {}",
        store_path.display()
    );

    let object_store: Arc<dyn ObjectStore> = if let Some(region) = &config.aws_region {
        // Configure AWS S3 if credentials are provided
        if let (Some(access_key), Some(secret_key)) =
            (&config.aws_access_key_id, &config.aws_secret_access_key)
        {
            let s3_config = object_store::aws::AmazonS3Builder::new()
                .with_region(region)
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key)
                .build()?;
            Arc::new(s3_config)
        } else {
            // Fall back to local filesystem if no AWS credentials
            Arc::new(
                object_store::local::LocalFileSystem::new_with_prefix(store_path)
                    .map_err(SearchEngineError::ObjectStoreError)?,
            )
        }
    } else {
        // Use local filesystem if no AWS region specified
        Arc::new(
            LocalFileSystem::new_with_prefix(store_path)
                .map_err(SearchEngineError::ObjectStoreError)?,
        )
    };

    Ok(object_store)
}

fn run() -> Result<()> {
    // Set up tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting Eureka Search Engine");

    // Parse command line arguments
    let args = CliArgs::parse();

    // Load configuration
    let config = get_config(args.config_path)?;

    // Create a runtime for async operations
    let rt = Runtime::new().map_err(|e| {
        SearchEngineError::IoError(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to create runtime: {}", e),
        ))
    })?;

    // Create a schema
    let schema = create_schema();

    // Set up object store
    let object_store = setup_object_store(&config)?;

    match args.command {
        Command::Index => {
            info!("Running indexing operation");

            // Create the search engine
            let search_engine = rt.block_on(async {
                SearchEngine::new(
                    object_store.clone(),
                    schema,
                    config,
                    "localhost:3000".to_string(),
                )
                .await
            })?;

            // Add documents
            rt.block_on(async { search_engine.add_documents(vec![]).await })?;

            // Persist to object store
            rt.block_on(async {
                search_engine.persist_to_store().await?;
                info!("Index successfully persisted to object store");
                Ok::<_, SearchEngineError>(())
            })?;
        }
        Command::Search { query } => {
            info!("Running search operation for query: {}", query);

            // Create the search engine
            let search_engine = rt.block_on(async {
                SearchEngine::new(
                    object_store.clone(),
                    schema,
                    config,
                    "localhost:3000".to_string(),
                )
                .await
            })?;

            rt.block_on(async {
                // Load the index from object store
                search_engine.load_from_store().await?;

                // Search
                let results = search_engine.search(&query)?;
                search_engine.print_search_results(&results, &query);

                Ok::<_, SearchEngineError>(())
            })?;
        }
        Command::Serve { port } => {
            info!("Starting API server on port {}", port);

            // Create the search engine
            let search_engine = rt.block_on(async {
                SearchEngine::new(
                    object_store.clone(),
                    schema,
                    config.clone(),
                    format!("localhost:{}", port),
                )
                .await
            })?;
            let search_engine = Arc::new(search_engine);

            // Load the index from object store
            rt.block_on(async {
                info!("Loading index data from object store");
                search_engine.load_from_store().await?;
                info!("Index successfully loaded from object store");
                Ok::<_, SearchEngineError>(())
            })?;

            // Create API state and router
            let api_state = api::ApiState {
                search_engine: search_engine.clone(),
            };
            let app = api::create_router(api_state);

            // Start the server with TLS if configured
            rt.block_on(async {
                let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
                    .await
                    .map_err(|e| {
                        SearchEngineError::IoError(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to bind to port {}: {}", port, e),
                        ))
                    })?;

                info!("API server listening on http://0.0.0.0:{}", port);
                axum::serve(listener, app).await.map_err(|e| {
                    SearchEngineError::IoError(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Server error: {}", e),
                    ))
                })?;

                Ok::<_, SearchEngineError>(())
            })?;
        }
    }

    info!("Operation completed successfully");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        error!("Error: {}", e);
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
