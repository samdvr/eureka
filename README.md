# Eureka Search Engine

A distributed search engine with etcd-based consensus, object store persistence, and a RESTful API.

## Features

- Full-text search using Tantivy
- Distributed consensus using etcd for leader election
- Object store persistence (local filesystem, with AWS S3 support)
- RESTful API with basic health checks and cluster status
- Configurable index buffer size and search result limits
- Leader election with automatic failover

## Prerequisites

- Rust 1.75 or later
- etcd (single node or cluster)
- Storage (local filesystem or S3)

## Installation

1. Clone the repository:

   ```bash
   git clone https://github.com/yourusername/eureka.git
   cd eureka
   ```

2. Build the project:

   ```bash
   cargo build --release
   ```

## Configuration

Create a `config.json` file with the following structure:

```json
{
  "index_buffer_size": 50000000,
  "storage_path": "object_store",
  "index_prefix": "index_data",
  "max_search_results": 100,
  "etcd_endpoints": ["http://localhost:2379"],
  "heartbeat_interval_secs": 5,
  "lock_ttl_secs": 30,
  "retry_interval_secs": 1
}
```

### Configuration Options

| Option                    | Description                                | Default                   |
| ------------------------- | ------------------------------------------ | ------------------------- |
| `index_buffer_size`       | Size of the index buffer in bytes          | 50MB                      |
| `storage_path`            | Path to the object store                   | "object_store"            |
| `index_prefix`            | Prefix for index files in the object store | "index_data"              |
| `max_search_results`      | Maximum number of results to return        | 100                       |
| `etcd_endpoints`          | List of etcd endpoints                     | ["http://localhost:2379"] |
| `heartbeat_interval_secs` | Interval between heartbeats                | 5                         |
| `lock_ttl_secs`           | Time-to-live for leader lock               | 30                        |
| `retry_interval_secs`     | Interval between leader election retries   | 1                         |
| `auth_enabled`            | Enable API authentication                  | false                     |
| `auth_token`              | API authentication token                   | null                      |

## Usage

### Starting the API Server

```bash
cargo run --release -- --config-path config.json serve --port 3000
```

The server will:

1. Connect to the etcd cluster
2. Participate in leader election
3. Load the index from the object store
4. Start the REST API server

### Indexing Documents

```bash
cargo run --release -- --config-path config.json index
```

### Searching Documents

```bash
cargo run --release -- --config-path config.json search "your query"
```

## API Endpoints

| Endpoint                   | Method | Description                    |
| -------------------------- | ------ | ------------------------------ |
| `/search?q=query&limit=10` | GET    | Search for documents           |
| `/index`                   | POST   | Index new documents            |
| `/health`                  | GET    | Health check endpoint          |
| `/cluster/status`          | GET    | Get cluster status information |

### API Examples

```bash
# Search
curl "http://localhost:3000/search?q=your+query&limit=10"

# Index documents
curl -X POST http://localhost:3000/index \
  -H "Content-Type: application/json" \
  -d '{
    "documents": [
      {
        "title": "Example Document",
        "body": "This is an example document to index."
      }
    ]
  }'

# Health check
curl http://localhost:3000/health

# Cluster status
curl http://localhost:3000/cluster/status
```

## Basic Deployment

### etcd Setup

For a single-node etcd instance (development):

```bash
etcd --listen-client-urls http://localhost:2379 \
     --advertise-client-urls http://localhost:2379
```

For production, a multi-node etcd cluster is recommended.

### Basic High Availability

1. Deploy multiple instances of the search engine pointing to the same etcd cluster
2. Use a simple load balancer (like Nginx or HAProxy) to distribute requests
3. Only the leader node will handle write operations (indexing)
4. All nodes can handle read operations (searching)

## Future Enhancements

The following features are planned for future releases:

- TLS support for secure etcd communication
- Full AWS S3, Google Cloud Storage, and Azure Blob Storage support
- Scatter-gather search across cluster nodes
- Metrics and monitoring integration
- Index optimization commands
- Enhanced security features
- Comprehensive backup and recovery procedures

## Development

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release
```

### Testing

```bash
# Run all tests
cargo test

# Run specific tests
cargo test search
```

## License

This project is open source and available under the [MIT License](LICENSE).

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
