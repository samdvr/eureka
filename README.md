# Eureka Search Engine

A search engine with distributed consensus using etcd, object store persistence, and a RESTful API.

## Features

- Full-text search using Tantivy
- Distributed consensus using etcd for leader election and cluster coordination
- Object store persistence (supports local filesystem, AWS S3, Google Cloud Storage, Azure Blob Storage)
- RESTful API with health checks and cluster status
- Configurable index buffer size and search limits
- High availability with automatic failover
- Scatter-gather search across cluster nodes

## Prerequisites

- Rust 1.75 or later
- etcd cluster (for distributed consensus)
- Object store (local filesystem, S3, GCS, or Azure Blob Storage)
- Load balancer (e.g., HAProxy, Nginx, or cloud provider's load balancer)

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
  "etcd_endpoints": [
    "http://localhost:2379",
    "http://localhost:2380",
    "http://localhost:2381"
  ],
  "heartbeat_interval_secs": 5,
  "lock_ttl_secs": 30,
  "retry_interval_secs": 1,
  "scatter_gather_enabled": true
}
```

Configuration options:

- `index_buffer_size`: Size of the index buffer in bytes (default: 50MB)
- `storage_path`: Path to the object store (default: "object_store")
- `index_prefix`: Prefix for index files in the object store (default: "index_data")
- `max_search_results`: Maximum number of results to return (default: 100)
- `etcd_endpoints`: List of etcd cluster endpoints (default: ["http://localhost:2379"])
- `heartbeat_interval_secs`: Interval between heartbeats (default: 5)
- `lock_ttl_secs`: Time-to-live for leader lock (default: 30)
- `retry_interval_secs`: Interval between leader election retries (default: 1)
- `scatter_gather_enabled`: Enable distributed search across cluster (default: false)

## Usage

### Starting the API Server

```bash
cargo run --release -- serve --port 3000
```

The server will:

1. Connect to the etcd cluster
2. Participate in leader election
3. Load the index from the object store
4. Start the REST API server

### Indexing Documents

```bash
cargo run --release -- index
```

This will:

1. Create a new index
2. Add sample documents
3. Persist the index to the object store

### Searching Documents

```bash
cargo run --release -- search "your query"
```

## API Endpoints

- `GET /search?q=query&limit=10&force_distributed=false`: Search for documents
  - Optional `force_distributed` parameter to force scatter-gather search
- `GET /search/distributed?q=query&limit=10`: Force distributed search across all nodes
- `POST /index`: Index new documents
- `GET /health`: Health check endpoint
- `GET /cluster/status`: Get cluster status and leadership information

### Example API Usage

```bash
# Regular search
curl "http://localhost:3000/search?q=your+query&limit=10"

# Search with forced distributed execution (scatter-gather)
curl "http://localhost:3000/search?q=your+query&limit=10&force_distributed=true"

# Explicitly use distributed search endpoint
curl "http://localhost:3000/search/distributed?q=your+query&limit=10"

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

# Check cluster status
curl http://localhost:3000/cluster/status

# Health check
curl http://localhost:3000/health
```

## Production Deployment

### etcd Setup

1. Deploy a 3-node etcd cluster for high availability:

   ```bash
   # Node 1
   etcd --name node1 \
     --initial-advertise-peer-urls http://10.0.1.10:2380 \
     --listen-peer-urls http://10.0.1.10:2380 \
     --listen-client-urls http://10.0.1.10:2379 \
     --advertise-client-urls http://10.0.1.10:2379 \
     --initial-cluster-token eureka-cluster \
     --initial-cluster node1=http://10.0.1.10:2380,node2=http://10.0.1.11:2380,node3=http://10.0.1.12:2380 \
     --initial-cluster-state new

   # Node 2
   etcd --name node2 \
     --initial-advertise-peer-urls http://10.0.1.11:2380 \
     --listen-peer-urls http://10.0.1.11:2380 \
     --listen-client-urls http://10.0.1.11:2379 \
     --advertise-client-urls http://10.0.1.11:2379 \
     --initial-cluster-token eureka-cluster \
     --initial-cluster node1=http://10.0.1.10:2380,node2=http://10.0.1.11:2380,node3=http://10.0.1.12:2380 \
     --initial-cluster-state new

   # Node 3
   etcd --name node3 \
     --initial-advertise-peer-urls http://10.0.1.12:2380 \
     --listen-peer-urls http://10.0.1.12:2380 \
     --listen-client-urls http://10.0.1.12:2379 \
     --advertise-client-urls http://10.0.1.12:2379 \
     --initial-cluster-token eureka-cluster \
     --initial-cluster node1=http://10.0.1.10:2380,node2=http://10.0.1.11:2380,node3=http://10.0.1.12:2380 \
     --initial-cluster-state new
   ```

2. Configure TLS for secure communication:

   ```bash
   # Generate certificates
   etcdctl --endpoints=https://10.0.1.10:2379 \
     --cacert=/path/to/ca.crt \
     --cert=/path/to/etcd.crt \
     --key=/path/to/etcd.key \
     member list
   ```

3. Set up monitoring and backup procedures:

   ```bash
   # Backup etcd data
   etcdctl --endpoints=https://10.0.1.10:2379 \
     --cacert=/path/to/ca.crt \
     --cert=/path/to/etcd.crt \
     --key=/path/to/etcd.key \
     snapshot save backup.db

   # Restore from backup
   etcdctl --endpoints=https://10.0.1.10:2379 \
     --cacert=/path/to/ca.crt \
     --cert=/path/to/etcd.crt \
     --key=/path/to/etcd.key \
     snapshot restore backup.db
   ```

### Object Store Configuration

For production, use a cloud object store:

```json
{
  "storage_path": "s3://your-bucket",
  "index_prefix": "index_data",
  "aws_region": "us-west-2",
  "aws_access_key_id": "YOUR_ACCESS_KEY",
  "aws_secret_access_key": "YOUR_SECRET_KEY"
}
```

### High Availability Setup

1. Deploy multiple instances of the search engine:

   ```bash
   # Node 1 (Leader)
   cargo run --release -- serve \
     --port 3000 \
     --config config.json \
     --address 10.0.1.20:3000

   # Node 2 (Follower)
   cargo run --release -- serve \
     --port 3000 \
     --config config.json \
     --address 10.0.1.21:3000

   # Node 3 (Follower)
   cargo run --release -- serve \
     --port 3000 \
     --config config.json \
     --address 10.0.1.22:3000
   ```

2. Configure load balancing:

   ```nginx
   # Nginx configuration
   upstream eureka_backend {
       server 10.0.1.20:3000;
       server 10.0.1.21:3000;
       server 10.0.1.22:3000;
   }

   server {
       listen 80;
       server_name search.example.com;

       location / {
           proxy_pass http://eureka_backend;
           proxy_set_header Host $host;
           proxy_set_header X-Real-IP $remote_addr;
           proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
           proxy_set_header X-Forwarded-Proto $scheme;
       }
   }
   ```

3. Set up monitoring and alerting:

   ```yaml
   # Prometheus configuration
   scrape_configs:
     - job_name: "eureka"
       static_configs:
         - targets: ["10.0.1.20:3000", "10.0.1.21:3000", "10.0.1.22:3000"]
       metrics_path: "/metrics"
   ```

4. Implement proper logging and tracing:
   ```json
   {
     "log_level": "info",
     "tracing_enabled": true,
     "tracing_endpoint": "http://jaeger:14268/api/traces"
   }
   ```

### Security Considerations

1. Use TLS for API endpoints:

   ```nginx
   server {
       listen 443 ssl;
       server_name search.example.com;
       ssl_certificate /path/to/cert.pem;
       ssl_certificate_key /path/to/key.pem;
       # ... rest of configuration
   }
   ```

2. Implement authentication and authorization:

   ```json
   {
     "auth_enabled": true,
     "auth_token": "your-secure-token"
   }
   ```

3. Secure etcd communication:

   ```json
   {
     "etcd_endpoints": [
       "https://10.0.1.10:2379",
       "https://10.0.1.11:2379",
       "https://10.0.1.12:2379"
     ],
     "etcd_ca_cert": "/path/to/ca.crt",
     "etcd_cert": "/path/to/etcd.crt",
     "etcd_key": "/path/to/etcd.key"
   }
   ```

4. Use secure credentials management:

   - Use AWS IAM roles for S3 access
   - Use Kubernetes secrets for sensitive data
   - Rotate credentials regularly

5. Regular security updates:
   - Keep dependencies up to date
   - Monitor security advisories
   - Regular penetration testing

## Monitoring and Maintenance

- Monitor etcd cluster health:

  ```bash
  etcdctl --endpoints=https://10.0.1.10:2379 \
    --cacert=/path/to/ca.crt \
    --cert=/path/to/etcd.crt \
    --key=/path/to/etcd.key \
    endpoint health
  ```

- Track index size and performance:

  ```bash
  # Monitor index size
  aws s3 ls s3://your-bucket/index_data/ --recursive

  # Monitor search latency
  curl -s http://localhost:3000/metrics | grep search_latency
  ```

- Monitor API latency and error rates:

  ```bash
  # Using Prometheus
  rate(http_requests_total[5m])
  rate(http_requests_errors_total[5m])
  ```

- Regular index optimization:

  ```bash
  # Merge segments
  cargo run --release -- optimize-index
  ```

- Backup and recovery procedures:

  ```bash
  # Backup index
  aws s3 sync s3://your-bucket/index_data/ s3://backup-bucket/index_data/

  # Restore index
  aws s3 sync s3://backup-bucket/index_data/ s3://your-bucket/index_data/
  ```

## Contributing

1. Fork the repository
2. Create your feature branch
3. Commit your changes
4. Push to the branch
5. Create a new Pull Request

## License

This project is licensed under the MIT License - see the LICENSE file for details.
