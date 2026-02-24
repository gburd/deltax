import os
import subprocess
import time
import uuid

import psycopg
import pytest

CONTAINER_NAME = "pg_cocoon_inttest"
HOST_PORT = 15432
PG_PASSWORD = "postgres"
PG_USER = "postgres"


@pytest.fixture(scope="session")
def pg_container():
    """Start the runtime container, wait for PG readiness, yield, then tear down."""
    image = os.environ.get("PG_COCOON_IMAGE")
    if not image:
        pytest.skip("PG_COCOON_IMAGE not set")

    # Clean up any leftover container from a previous run
    subprocess.run(
        ["docker", "rm", "-f", CONTAINER_NAME],
        capture_output=True,
    )

    # Start container
    subprocess.check_call(
        [
            "docker", "run", "-d",
            "--name", CONTAINER_NAME,
            "-p", f"{HOST_PORT}:5432",
            "-e", f"POSTGRES_PASSWORD={PG_PASSWORD}",
            image,
        ]
    )

    # Wait for readiness
    _wait_for_pg()

    yield

    # Teardown
    subprocess.run(["docker", "rm", "-f", CONTAINER_NAME], capture_output=True)


def _wait_for_pg(timeout: int = 30):
    """Poll pg_isready until the container is accepting connections."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        result = subprocess.run(
            [
                "docker", "exec", CONTAINER_NAME,
                "pg_isready", "-U", PG_USER,
            ],
            capture_output=True,
        )
        if result.returncode == 0:
            return
        time.sleep(1)
    raise TimeoutError(f"PostgreSQL not ready after {timeout}s")


def _admin_conn():
    """Return a connection to the default 'postgres' database with autocommit."""
    conn = psycopg.connect(
        host="localhost",
        port=HOST_PORT,
        user=PG_USER,
        password=PG_PASSWORD,
        dbname="postgres",
        autocommit=True,
    )
    return conn


@pytest.fixture()
def db(pg_container):
    """Create a fresh test database with the extension, yield a connection, then drop it."""
    db_name = "test_" + uuid.uuid4().hex[:12]

    admin = _admin_conn()
    admin.execute(f'CREATE DATABASE "{db_name}"')
    admin.close()

    conn = psycopg.connect(
        host="localhost",
        port=HOST_PORT,
        user=PG_USER,
        password=PG_PASSWORD,
        dbname=db_name,
    )
    conn.execute("CREATE EXTENSION pg_cocoon")
    conn.commit()

    yield conn

    conn.close()

    admin = _admin_conn()
    admin.execute(f'DROP DATABASE "{db_name}"')
    admin.close()
