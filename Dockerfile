ARG PG_MAJOR=17

# ==============================================================================
# Stage 1: Build environment
# ==============================================================================
FROM rust:1-bookworm AS builder

ARG PG_MAJOR=17

# Install PostgreSQL from PGDG
RUN apt-get update && apt-get install -y --no-install-recommends \
        gnupg2 curl ca-certificates lsb-release \
    && echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list \
    && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        | gpg --dearmor -o /etc/apt/trusted.gpg.d/pgdg.gpg \
    && apt-get update && apt-get install -y --no-install-recommends \
        postgresql-${PG_MAJOR} \
        postgresql-server-dev-${PG_MAJOR} \
        pkg-config \
        libssl-dev \
        libclang-dev \
        clang \
    && rm -rf /var/lib/apt/lists/*

# Make pg_config available
ENV PATH="/usr/lib/postgresql/${PG_MAJOR}/bin:${PATH}"

# Install cargo-pgrx matching our dependency version
RUN cargo install cargo-pgrx --version "0.17.0" --locked

# Initialize pgrx with system PostgreSQL
RUN cargo pgrx init --pg${PG_MAJOR}=/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

WORKDIR /build/pg_cocoon

# Cache dependency builds: copy only manifests first
COPY Cargo.toml Cargo.lock* ./
COPY src/bin/pgrx_embed.rs src/bin/pgrx_embed.rs
RUN mkdir -p src && echo '#[allow(unused)] fn main() {}' > src/lib.rs \
    && cargo fetch

# Copy full source
COPY . .

# Build the extension
RUN cargo pgrx package --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config \
        --features pg${PG_MAJOR} --no-default-features

# ==============================================================================
# Stage 2: Test runner (includes full PG server for cargo pgrx test)
# ==============================================================================
FROM rust:1-bookworm AS test

ARG PG_MAJOR=17

RUN apt-get update && apt-get install -y --no-install-recommends \
        gnupg2 curl ca-certificates lsb-release sudo \
    && echo "deb http://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" \
        > /etc/apt/sources.list.d/pgdg.list \
    && curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        | gpg --dearmor -o /etc/apt/trusted.gpg.d/pgdg.gpg \
    && apt-get update && apt-get install -y --no-install-recommends \
        postgresql-${PG_MAJOR} \
        postgresql-server-dev-${PG_MAJOR} \
        pkg-config \
        libssl-dev \
        libclang-dev \
        clang \
    && rm -rf /var/lib/apt/lists/*

ENV PATH="/usr/lib/postgresql/${PG_MAJOR}/bin:${PATH}"
ENV PG_MAJOR=${PG_MAJOR}

RUN cargo install cargo-pgrx --version "0.17.0" --locked

# Create non-root user (initdb refuses to run as root)
# Give builder write access to PG extension dirs so `cargo pgrx test` can install the extension
RUN useradd -m -s /bin/bash builder \
    && mkdir -p /build/pg_cocoon \
    && chown -R builder:builder /build /usr/local/cargo \
        /usr/share/postgresql/${PG_MAJOR}/extension \
        /usr/lib/postgresql/${PG_MAJOR}/lib

USER builder
ENV CARGO_HOME=/usr/local/cargo
ENV USER=builder
RUN cargo pgrx init --pg${PG_MAJOR}=/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

WORKDIR /build/pg_cocoon
COPY --chown=builder:builder . .

CMD ["sh", "-c", "cargo pgrx test pg${PG_MAJOR}"]

# ==============================================================================
# Stage 3: Minimal runtime image with extension installed
# ==============================================================================
FROM postgres:${PG_MAJOR}-bookworm AS runtime

ARG PG_MAJOR=17

COPY --from=builder /build/pg_cocoon/target/release/pg_cocoon-pg${PG_MAJOR}/ /

# Extension is now available: CREATE EXTENSION pg_cocoon;
