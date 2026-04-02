#!/bin/bash
# Full EC2 benchmark setup: install deps, build extension, download data, load, compress.
# Expects pg_deltax source already at ~/pg_deltax (synced via `make deploy`).
# Expects this script to run from ~/clickbench on the EC2 instance.
#
# Idempotent: skips download/split if data chunks already exist.
# Drops and recreates the DB so it can be re-run for recompression.

set -euo pipefail

PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config
DB=test
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SPLIT_DIR=/tmp/hits_chunks
LOAD_WORKERS=8
COMPRESS_WORKERS=16

# Install PostgreSQL 18
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -y
sudo apt-get install -y gnupg postgresql-common apt-transport-https lsb-release wget pigz parallel
sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
sudo apt-get update -y
sudo apt-get install -y postgresql-18 postgresql-client-18

# Install Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Install build dependencies
sudo apt-get install -y pkg-config libssl-dev libclang-dev clang postgresql-server-dev-18

# Install cargo-pgrx
cargo install cargo-pgrx --version 0.17.0 --locked

# Build and install pg_deltax (source already synced by Makefile)
cd ~/pg_deltax
cargo pgrx init --pg18 "$PG_CONFIG"
sudo env "PATH=$PATH" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}" "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "PGRX_HOME=$HOME/.pgrx" \
    cargo pgrx install --pg-config "$PG_CONFIG" --release
cd "$SCRIPT_DIR"

# Configure PostgreSQL (idempotent: only add if not already present)
if ! sudo grep -q "shared_preload_libraries.*pg_deltax" /etc/postgresql/18/main/postgresql.conf; then
    sudo bash -c "echo \"shared_preload_libraries = 'pg_deltax'\" >> /etc/postgresql/18/main/postgresql.conf"
fi
sudo systemctl restart postgresql

# Drop and recreate the database (allows re-running for recompression)
sudo -u postgres psql -c "DROP DATABASE IF EXISTS $DB"
sudo -u postgres psql -c "CREATE DATABASE $DB"
sudo -u postgres psql "$DB" -c "CREATE EXTENSION pg_deltax"
sudo -u postgres psql -c "ALTER DATABASE $DB SET work_mem TO '1GB'"
sudo -u postgres psql -c "ALTER DATABASE $DB SET min_parallel_table_scan_size TO '0'"

# Download and split data (skip if chunks already exist)
if [ -d "$SPLIT_DIR" ] && ls "$SPLIT_DIR"/chunk_* &>/dev/null; then
    echo "Reusing existing data chunks in $SPLIT_DIR"
    TOTAL_LINES=$(wc -l "$SPLIT_DIR"/chunk_* | tail -1 | awk '{print $1}')
else
    # Download data
    if [ ! -f /tmp/hits.tsv ]; then
        wget --continue --progress=dot:giga 'https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz'
        pigz -d -f hits.tsv.gz
        sudo mv hits.tsv /tmp/hits.tsv
        sudo chmod 644 /tmp/hits.tsv
    fi

    # Split TSV into chunks for parallel loading
    echo "Splitting data into $LOAD_WORKERS chunks..."
    sudo rm -rf "$SPLIT_DIR"
    sudo mkdir -p "$SPLIT_DIR"
    TOTAL_LINES=$(wc -l < /tmp/hits.tsv)
    LINES_PER_CHUNK=$(( (TOTAL_LINES + LOAD_WORKERS - 1) / LOAD_WORKERS ))
    sudo split -l "$LINES_PER_CHUNK" -d -a 2 /tmp/hits.tsv "$SPLIT_DIR/chunk_"
    sudo chmod 644 "$SPLIT_DIR"/chunk_*
    echo "Split into $(ls "$SPLIT_DIR" | wc -l) chunks of ~$LINES_PER_CHUNK lines each"
fi

# Create table
sudo -u postgres psql "$DB" < create.sql 2>&1 | tee load_out.txt
if grep 'ERROR' load_out.txt; then
    exit 1
fi

# Set up partitioning — mock_now must be set before deltax_create_table
sudo -u postgres psql "$DB" -t -c "SET pg_deltax.mock_now = '2013-07-01 12:00:00'; SELECT deltax_create_table('hits', 'eventtime', '3 days'::interval, 15)"

# Parallel data loading
echo "Loading data with $LOAD_WORKERS parallel workers..."
LOAD_START=$(date +%s)
ls "$SPLIT_DIR"/chunk_* \
    | parallel -j "$LOAD_WORKERS" \
        "sudo -u postgres psql $DB -c \"\\copy hits FROM '{}'\""
LOAD_END=$(date +%s)
echo "Load time: $((LOAD_END - LOAD_START))s ($TOTAL_LINES rows, $LOAD_WORKERS workers)"

# Enable compression
sudo -u postgres psql "$DB" -t -c "SELECT deltax_enable_compression('hits', order_by => ARRAY['counterid', 'userid', 'eventtime'], segment_size => 30000)"

# Parallel compression
echo "Compressing partitions with $COMPRESS_WORKERS parallel workers..."
COMPRESS_START=$(date +%s)
sudo -u postgres psql "$DB" -t -A -c \
    "SELECT partition_name FROM deltax_partition_info('hits') WHERE partition_name NOT LIKE '%default%'" \
    | grep -v '^$' \
    | parallel -j "$COMPRESS_WORKERS" \
        "sudo -u postgres psql $DB -q -c \"SELECT deltax_compress_partition('{}')\" && echo '  Compressed {}'"
COMPRESS_END=$(date +%s)
echo "Compress time: $((COMPRESS_END - COMPRESS_START))s ($COMPRESS_WORKERS workers)"

# Vacuum
echo -n "Vacuum time: "
VACUUM_START=$(date +%s)
sudo -u postgres psql "$DB" -q -t -c "VACUUM FREEZE ANALYZE hits"
VACUUM_END=$(date +%s)
echo "Vacuum time: $((VACUUM_END - VACUUM_START))s"

# Keep chunks for potential re-runs (they're ~14GB but save 5+ min of re-splitting)

# Capture data size (bytes)
DATA_SIZE=$(sudo -u postgres psql "$DB" -t -A -c "SELECT pg_database_size('$DB')")
echo "Data size: $DATA_SIZE bytes ($(echo "$DATA_SIZE / 1024 / 1024 / 1024" | bc -l | xargs printf '%.2f') GB)"

# Save load stats for the bench target to pick up later
LOAD_TIME=$((LOAD_END - LOAD_START + COMPRESS_END - COMPRESS_START + VACUUM_END - VACUUM_START))
cat > ~/clickbench/load_stats.env <<STATS
LOAD_TIME=$LOAD_TIME
DATA_SIZE=$DATA_SIZE
STATS
echo "Saved load stats to ~/clickbench/load_stats.env (load_time=${LOAD_TIME}s, data_size=${DATA_SIZE})"

# Lower work_mem and disable JIT for the query phase
sudo -u postgres psql -c "ALTER DATABASE $DB SET work_mem TO '256MB'"
sudo -u postgres psql -c "ALTER DATABASE $DB SET jit TO off"

# Report partition and compression info
sudo -u postgres psql "$DB" -c "SELECT * FROM deltax_partition_info('hits')"
sudo -u postgres psql "$DB" -c "SELECT count(*) AS default_partition_rows FROM hits_default"

echo "Setup complete. Database '$DB' is ready."
echo "Run queries manually with: sudo -u postgres psql $DB -c '\timing' -c 'QUERY'"
echo "Or run all queries with: ./run.sh"
