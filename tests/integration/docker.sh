#!/usr/bin/env bash
# Requires Docker, Python 3, Cargo. All database client tools run in test containers.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
export DBFORK_TEST_CONTAINER_PG="dbfork-test-pg-$$"
export DBFORK_TEST_CONTAINER_MYSQL="dbfork-test-mysql-$$"
export DBFORK_TEST_CONTAINER_MONGO="dbfork-test-mongo-$$"
redis_container="dbfork-test-redis-$$"
mariadb_container="dbfork-test-mariadb-$$"
cleanup() {
  docker rm -f "$DBFORK_TEST_CONTAINER_PG" "$DBFORK_TEST_CONTAINER_MYSQL" "$DBFORK_TEST_CONTAINER_MONGO" "$redis_container" "$mariadb_container" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
for tool in pg_dump pg_restore mysql mysqldump mariadb mariadb-dump mongosh mongodump mongorestore; do
  ln -s "$PWD/tests/integration/docker-client.py" "$work/$tool"
done
export PATH="$work:$PATH"
docker run -d --name "$DBFORK_TEST_CONTAINER_PG" -p 127.0.0.1::5432 -e POSTGRES_PASSWORD=dbfork_test -e POSTGRES_DB=source postgres:17-alpine >/dev/null
docker run -d --name "$DBFORK_TEST_CONTAINER_MYSQL" -p 127.0.0.1::3306 -e MYSQL_ROOT_PASSWORD=dbfork_test -e MYSQL_DATABASE=source mysql:8.4 >/dev/null
# MongoDB 8 currently rejects some Docker Desktop kernels (SERVER-121912).
docker run -d --name "$DBFORK_TEST_CONTAINER_MONGO" -p 127.0.0.1::27017 mongo:7 >/dev/null
docker run -d --name "$mariadb_container" -p 127.0.0.1::3306 -e MARIADB_ROOT_PASSWORD=dbfork_test -e MARIADB_DATABASE=source mariadb:11.4 >/dev/null
docker run -d --name "$redis_container" -p 127.0.0.1::6379 redis:7-alpine >/dev/null
wait_for() {
  local container="$1"; shift
  for attempt in {1..90}; do
    if docker exec "$container" "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  docker logs --tail 20 "$container"
  echo "Timed out waiting for $container" >&2
  return 1
}
wait_for "$DBFORK_TEST_CONTAINER_PG" pg_isready -U postgres
wait_for "$DBFORK_TEST_CONTAINER_MYSQL" mysql -uroot --password=dbfork_test -e 'SELECT 1'
wait_for "$DBFORK_TEST_CONTAINER_MONGO" mongosh --quiet --eval 'db.runCommand({ping:1})'
wait_for "$redis_container" redis-cli ping
wait_for "$mariadb_container" mariadb -uroot --password=dbfork_test -e 'SELECT 1'
port() { docker port "$1" "$2/tcp" | head -1 | sed 's/.*://'; }
export DBFORK_TEST_POSTGRES="postgres://postgres:dbfork_test@127.0.0.1:$(port "$DBFORK_TEST_CONTAINER_PG" 5432)/source"
export DBFORK_TEST_MYSQL="mysql://root:dbfork_test@127.0.0.1:$(port "$DBFORK_TEST_CONTAINER_MYSQL" 3306)/source"
export DBFORK_TEST_MYSQL_FLAVOR=mysql
export DBFORK_TEST_MONGODB="mongodb://127.0.0.1:$(port "$DBFORK_TEST_CONTAINER_MONGO" 27017)/source"
export DBFORK_TEST_REDIS="redis://127.0.0.1:$(port "$redis_container" 6379)/0"
cargo test -p dbfork-engines --test network -- --ignored --test-threads=1

export DBFORK_TEST_MYSQL="mysql://root:dbfork_test@127.0.0.1:$(port "$mariadb_container" 3306)/source"
export DBFORK_TEST_MYSQL_FLAVOR=mariadb
DBFORK_TEST_CONTAINER_MYSQL="$mariadb_container" cargo test -p dbfork-engines --test network mysql_full_schema_seed_and_rollback -- --ignored
