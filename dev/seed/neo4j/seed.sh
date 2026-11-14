#!/bin/sh
# Loads the "nine" dataset into Neo4j. Run by the neo4j-seed service:
#   seed.sh <bolt-url>
# Requires NEO4J_USERNAME / NEO4J_PASSWORD. Idempotent: skips when the
# :NineMeta node exists.
set -eu
URL="$1"
if cypher-shell -a "$URL" --format plain "MATCH (m:NineMeta) RETURN count(m) AS n" | tail -n 1 | grep -q '^1$'; then
  echo "nine dataset already present at $URL, skipping"
  exit 0
fi
cypher-shell -a "$URL" -f "$(dirname "$0")/nine.cypher"
echo "nine dataset loaded at $URL"
