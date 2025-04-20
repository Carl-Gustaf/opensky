FROM ubuntu:20.04

# Install dependencies
RUN apt-get update && apt-get install -y \
    curl \
    python3 \
    python3-pip \
    docker.io \
    libssl-dev \
    build-essential

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Install Substrate dependencies
RUN rustup default stable && \
    rustup update && \
    rustup update nightly && \
    rustup target add wasm32-unknown-unknown --toolchain nightly

# Copy OpenSky client code
WORKDIR /opensky
COPY . .

# Build the client
RUN cargo build --release

# Expose P2P port
EXPOSE 30333

# Default resource allocation (configurable via env vars)
ENV OPENSKY_MAX_CPU_PERCENT=50
ENV OPENSKY_MAX_STORAGE_GB=10
ENV OPENSKY_MAX_BANDWIDTH_MBPS=50

# Run the client
CMD ["./target/release/opensky-client"]
