#!/usr/bin/env bash
# Orchestrate the cc32d9 stack in one container: MariaDB → schema/network → DB writer (WS sink) →
# Chronicle (reads nodeos SHiP, exports to the writer) → Starman HTTP API.
set -euo pipefail

NETWORK=libre
DBWRITE_PORT=8105
SHIP_HOST="${NODEOS_HOST:-cc32d9-nodeos}"
SHIP_PORT="${SHIP_PORT:-8080}"
START_BLOCK="$(cat /snapshot/BLOCK_NUM 2>/dev/null || echo 0)"

if ! [[ "$START_BLOCK" =~ ^[0-9]+$ ]] || [[ "$START_BLOCK" -le 0 ]]; then
  echo "[cc32d9] ERROR: snapshot start block unknown (/snapshot/BLOCK_NUM='$START_BLOCK')."
  echo "[cc32d9] Chronicle needs the snapshot block; check the snapshot-fetch step."
  exit 1
fi

# ── 1. MariaDB ──────────────────────────────────────────────────────────────────────────────────
chown -R mysql:mysql /var/lib/mysql
if [[ ! -d /var/lib/mysql/mysql ]]; then
  echo "[cc32d9] initializing MariaDB data dir"
  mariadb-install-db --user=mysql --datadir=/var/lib/mysql --auth-root-authentication-method=normal >/dev/null
fi
echo "[cc32d9] starting MariaDB"
mysqld_safe --datadir=/var/lib/mysql --skip-networking=0 &
for i in $(seq 1 60); do mysqladmin ping --silent 2>/dev/null && break; sleep 1; done
mysqladmin ping --silent || { echo "[cc32d9] MariaDB failed to start"; exit 1; }

# ── 2. schema + network registration (idempotent) ────────────────────────────────────────────────
if ! mysql -e 'use lightapi' 2>/dev/null; then
  echo "[cc32d9] creating schema + registering $NETWORK"
  ( cd /opt/eosio_light_api/sql && mysql < lightapi_dbcreate.sql && sh create_tables.sh "$NETWORK" )
  # The PSGI API connects as lightapiro@localhost over the socket; ensure that grant exists.
  mysql -e "CREATE USER IF NOT EXISTS 'lightapiro'@'localhost' IDENTIFIED BY 'lightapiro';
            GRANT SELECT ON lightapi.* TO 'lightapiro'@'localhost'; FLUSH PRIVILEGES;"
  ( cd /opt/eosio_light_api/setup && sh "add_${NETWORK}_mainnet.sh" )
else
  echo "[cc32d9] schema already present"
fi

# ── 3. DB writer (opens a WS port for Chronicle to push into) ─────────────────────────────────────
echo "[cc32d9] starting lightapi_dbwrite (ws :$DBWRITE_PORT)"
perl /opt/eosio_light_api/scripts/lightapi_dbwrite.pl --network="$NETWORK" --port="$DBWRITE_PORT" &
sleep 3

# ── 4. Chronicle (reads nodeos SHiP from START_BLOCK, exports to the writer) ──────────────────────
mkdir -p /srv/$NETWORK/chronicle-config /srv/$NETWORK/chronicle-data
sed -e "s|##SHIP_HOST##|$SHIP_HOST|" -e "s|##SHIP_PORT##|$SHIP_PORT|" -e "s|##DBWRITE_PORT##|$DBWRITE_PORT|" \
    /opt/chronicle-config.ini.template > /srv/$NETWORK/chronicle-config/config.ini

echo "[cc32d9] waiting for nodeos SHiP at $SHIP_HOST:$SHIP_PORT"
until nc -z "$SHIP_HOST" "$SHIP_PORT"; do sleep 5; done

echo "[cc32d9] starting chronicle-receiver (start-block=$START_BLOCK)"
# Retry loop: chronicle exits when the ship node stops sending new blocks; on restart it resumes from
# its own data-dir (start-block only applies to a fresh chronicle DB). Initial full-state load can
# take a while (it processes the snapshot block's entire-state delta).
(
  while true; do
    /usr/local/sbin/chronicle-receiver \
      --config-dir=/srv/$NETWORK/chronicle-config \
      --data-dir=/srv/$NETWORK/chronicle-data \
      --start-block="$START_BLOCK" || true
    echo "[cc32d9] chronicle-receiver exited; resuming in 15s"
    sleep 15
  done
) &

# ── 5. HTTP API (the benchmark target) ───────────────────────────────────────────────────────────
WORKERS="${STARMAN_WORKERS:-6}"
echo "[cc32d9] starting Starman HTTP API on :5001 (workers=$WORKERS)"
exec starman --listen 0.0.0.0:5001 --workers "$WORKERS" /opt/eosio_light_api/api/lightapi.psgi
