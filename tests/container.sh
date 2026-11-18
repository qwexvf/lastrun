#!/bin/bash
# Exercises the daemon without host root: a container with CAP_SYS_ADMIN marks only
# a tmpfs at /w, so nothing on the host is watched.
# Usage: tests/container.sh   (after cargo build --release)
set -euo pipefail
cd "$(dirname "$0")/.."
image=${IMAGE:-debian:trixie-slim}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

cat > "$out/inner.sh" <<'INNER'
#!/bin/bash
set -u
cp /bin/ls /bin/bash /bin/true /w/
printf '#!/w/bash\ntrue\n' > /w/probe.sh && chmod +x /w/probe.sh
mkdir -p /w/proj/target/release && cp /bin/true /w/proj/target/release/x
round() {
  /lastrun --db /out/u.db daemon --mount /w --flush 1 2>>/out/daemon.err &
  local d=$!
  sleep 1
  for _ in 1 2 3; do /w/ls / >/dev/null; done
  /w/probe.sh
  ls / >/dev/null                  # overlay, not marked
  /w/proj/target/release/x         # ignored build output
  local loops=()
  for _ in 1 2 3 4; do (for _ in $(seq 2500); do /w/true; done) & loops+=($!); done
  wait "${loops[@]}"
  sleep 2
  kill -TERM "$d"; wait "$d"
}
round; round
INNER
chmod +x "$out/inner.sh"

docker run --rm --cap-add SYS_ADMIN --tmpfs /w:exec \
  -v "$PWD/target/release/lastrun:/lastrun:ro" -v "$out:/out" "$image" /out/inner.sh

got=$(sqlite3 "$out/u.db" "SELECT path || '=' || count FROM usage ORDER BY path" | tr '\n' ' ')
want="/w/bash=2 /w/ls=6 /w/probe.sh=2 /w/true=20000 "
cat "$out/daemon.err"
if [[ $got == "$want" ]]; then echo "ok: $got"; else echo "FAIL: got '$got' want '$want'"; exit 1; fi
