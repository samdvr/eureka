use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use serde::{Deserialize, Serialize};
use tantivy::schema::Value;
use tower_http::trace::TraceLayer;
use tracing::{debug, info};

use crate::consensus::ClusterState;
use crate::SearchEngine;
use crate::{SearchEngineConfig, SearchEngineError};

#[derive(Clone)]
pub struct ApiState {
    pub search_engine: Arc<SearchEngine>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl From<SearchEngineError> for ApiError {
    fn from(err: SearchEngineError) -> Self {
        let status = match &err {
            SearchEngineError::ConfigError(_) => StatusCode::BAD_REQUEST,
            SearchEngineError::FieldNotFound(_) => StatusCode::BAD_REQUEST,
            SearchEngineError::QueryParseError(_) => StatusCode::BAD_REQUEST,
            SearchEngineError::ConsensusError(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        Self {
            status: status.as_u16(),
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Json(self);

        (status, body).into_response()
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub force_distributed: bool,
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub total: usize,
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub score: f32,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct IndexRequest {
    pub documents: Vec<DocumentToIndex>,
}

#[derive(Debug, Deserialize)]
pub struct DocumentToIndex {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct IndexResponse {
    pub status: String,
    pub documents_indexed: usize,
}

#[derive(Debug, Serialize)]
pub struct ClusterStatusResponse {
    pub is_leader: bool,
    pub cluster_state: ClusterState,
}

async fn check_auth(
    headers: HeaderMap,
    config: &SearchEngineConfig,
) -> std::result::Result<(), ApiError> {
    if !config.auth_enabled {
        return Ok(());
    }

    let auth_token = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError {
            status: StatusCode::UNAUTHORIZED.as_u16(),
            message: "Missing or invalid Authorization header".to_string(),
        })?;

    if Some(auth_token) != config.auth_token.as_deref() {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED.as_u16(),
            message: "Invalid authentication token".to_string(),
        });
    }

    Ok(())
}

pub fn create_router(state: ApiState) -> Router {
    Router::new()
        .route("/search", get(search))
        .route("/search/distributed", get(distributed_search))
        .route("/index", post(index))
        .route("/health", get(health_check))
        .route("/cluster/status", get(cluster_status))
        .layer(TraceLayer::new_for_http())
        .with_state(Arc::new(state))
}

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

async fn cluster_status(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> std::result::Result<Json<ClusterStatusResponse>, ApiError> {
    check_auth(headers, &state.search_engine.config).await?;

    let is_leader = state.search_engine.consensus.is_leader().await;
    let cluster_state = state.search_engine.consensus.get_cluster_state().await;

    Ok(Json(ClusterStatusResponse {
        is_leader,
        cluster_state,
    }))
}

async fn search(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> std::result::Result<Json<SearchResponse>, ApiError> {
    // Check authentication
    check_auth(headers, &state.search_engine.config).await?;

    info!("Search request received for query: {}", params.q);

    // Use a smaller limit if one was specified
    let limit = params
        .limit
        .min(state.search_engine.config.max_search_results);

    // Use the async search method that supports scatter-gather
    let search_engine = state.search_engine.clone();
    let query = params.q.clone();
    let results = (*search_engine)
        .search_async(&query, params.force_distributed)
        .await?;

    // Get the total count before applying limit
    let total = results.len();

    // Convert to response format
    let schema = state.search_engine.index.schema();
    let title_field = schema.get_field("title").unwrap();
    let body_field = schema.get_field("body").unwrap();

    let search_results: Vec<SearchResult> = results
        .into_iter()
        .take(limit)
        .map(|(score, doc)| {
            let title = doc
                .get_first(title_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let body = doc
                .get_first(body_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            SearchResult { score, title, body }
        })
        .collect();

    let response = SearchResponse {
        query: params.q,
        total,
        results: search_results,
    };

    Ok(Json(response))
}

// Dedicated endpoint for distributed search
async fn distributed_search(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> std::result::Result<Json<SearchResponse>, ApiError> {
    // Check authentication
    check_auth(headers, &state.search_engine.config).await?;

    info!(
        "Distributed search request received for query: {}",
        params.q
    );

    // Force scatter-gather for this endpoint regardless of global config
    if !state.search_engine.config.scatter_gather_enabled {
        info!("Scatter-gather not enabled in config, but forcing for distributed search endpoint");
    }

    // Use a smaller limit if one was specified
    let limit = params
        .limit
        .min(state.search_engine.config.max_search_results);

    // Always force distributed search for this endpoint
    let search_engine = state.search_engine.clone();
    let query = params.q.clone();
    let results = (*search_engine).search_async(&query, true).await?;

    // Get the total count before applying limit
    let total = results.len();

    // Convert to response format
    let schema = state.search_engine.index.schema();
    let title_field = schema.get_field("title").unwrap();
    let body_field = schema.get_field("body").unwrap();

    let search_results: Vec<SearchResult> = results
        .into_iter()
        .take(limit)
        .map(|(score, doc)| {
            let title = doc
                .get_first(title_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let body = doc
                .get_first(body_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            SearchResult { score, title, body }
        })
        .collect();

    let response = SearchResponse {
        query: params.q,
        total,
        results: search_results,
    };

    Ok(Json(response))
}

async fn index(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(index_request): Json<IndexRequest>,
) -> std::result::Result<Json<IndexResponse>, ApiError> {
    check_auth(headers, &state.search_engine.config).await?;

    info!(
        "Index request received for {} documents",
        index_request.documents.len()
    );

    let schema = state.search_engine.index.schema();
    let title_field = schema.get_field("title").unwrap();
    let body_field = schema.get_field("body").unwrap();

    // Convert to Tantivy documents
    let documents: Vec<tantivy::TantivyDocument> = index_request
        .documents
        .iter()
        .map(|doc| {
            let mut tantivy_doc = tantivy::TantivyDocument::default();
            tantivy_doc.add_text(title_field, &doc.title);
            tantivy_doc.add_text(body_field, &doc.body);
            tantivy_doc
        })
        .collect();

    // Index the documents
    state.search_engine.add_documents(documents).await?;

    // Persist the index to object store
    tokio::spawn({
        let engine = state.search_engine.clone();
        async move {
            if let Err(e) = engine.persist_to_store().await {
                tracing::error!("Failed to persist index: {}", e);
            } else {
                debug!("Index successfully persisted to object store");
            }
        }
    });

    let response = IndexResponse {
        status: "success".to_string(),
        documents_indexed: index_request.documents.len(),
    };

    Ok(Json(response))
}
