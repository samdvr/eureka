# Build stage
FROM rust:1.85.1-slim-bullseye AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
  pkg-config \
  libssl-dev \
  protobuf-compiler \
  && rm -rf /var/lib/apt/lists/*

# Create a new empty shell project
WORKDIR /usr/src/eureka
COPY . .

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bullseye-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
  ca-certificates \
  libssl1.1 \
  && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/eureka/target/release/eureka /usr/local/bin/eureka

# Create a non-root user
RUN useradd -m -u 1000 eureka

# Create config directory and set permissions
RUN mkdir -p /etc/eureka && \
  chown -R eureka:eureka /etc/eureka

# Set environment variables
ENV CONFIG_PATH=/etc/eureka/config.json
ENV API_PORT=3000

# Switch to non-root user
USER eureka

# Expose the API port
EXPOSE 3000

# Run the application
CMD ["eureka", "serve"] 