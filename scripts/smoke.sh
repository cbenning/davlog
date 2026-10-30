#!/usr/bin/env bash
# Smoke-test a built server with real WebDAV clients. Missing tools are skipped.
# Usage: scripts/smoke.sh [path-to-binary]   (default: target/debug/davlog)
set -u

BIN=${1:-target/debug/davlog}
PORT=${PORT:-18090}
WORK=$(mktemp -d)
trap 'kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null; rm -rf "$WORK"' EXIT

printf 'pw' > "$WORK/password"
cat > "$WORK/config.toml" <<CFG
[server]
listen = "127.0.0.1:$PORT"
repo_path = "$WORK/repo"
[auth]
username = "notes"
password_file = "$WORK/password"
CFG

"$BIN" --config "$WORK/config.toml" --bootstrap init 2> "$WORK/server.log" &
PID=$!
for _ in $(seq 50); do curl -s "http://127.0.0.1:$PORT/healthz" > /dev/null && break; sleep 0.1; done

URL="http://127.0.0.1:$PORT"
fail=0
check() { # check <description> <command...>
  local what=$1; shift
  if "$@" > "$WORK/last.out" 2>&1; then echo "ok   $what"; else echo "FAIL $what"; tail -5 "$WORK/last.out"; fail=1; fi
}

if command -v rclone > /dev/null; then
  mkdir -p "$WORK/src/dir/sub" "$WORK/back"
  echo one > "$WORK/src/a.md"; echo two > "$WORK/src/dir/b.md"; echo three > "$WORK/src/dir/sub/c.md"
  head -c 300000 /dev/urandom > "$WORK/src/blob.bin"
  R="rclone --webdav-url=$URL --webdav-user=notes --webdav-pass=$(rclone obscure pw) --webdav-vendor=other"
  check "rclone sync up"   $R sync "$WORK/src" :webdav:
  check "rclone sync down" $R sync :webdav: "$WORK/back"
  check "rclone round trip is identical" diff -r "$WORK/src" "$WORK/back"
  rm "$WORK/src/dir/b.md"; echo changed > "$WORK/src/a.md"
  check "rclone sync changes" $R sync "$WORK/src" :webdav:
else
  echo "skip rclone (not installed)"
fi

if command -v litmus > /dev/null; then
  check "litmus" litmus "$URL/" notes pw
else
  echo "skip litmus (not installed)"
fi

if command -v cadaver > /dev/null; then
  printf 'user notes\npw\n' > "$WORK/netrc"
  printf 'mkcol cad\nput /etc/hostname cad/host.txt\nls cad\nmove cad/host.txt cad/moved.txt\ndelete cad/moved.txt\n' > "$WORK/cadaver.cmds"
  check "cadaver" env HOME="$WORK" cadaver "$URL/" < "$WORK/cadaver.cmds"
else
  echo "skip cadaver (not installed)"
fi

echo "--- git log"
git -C "$WORK/repo" log --oneline | head -20
echo "commits: $(git -C "$WORK/repo" rev-list --count HEAD)"
check "working tree is clean after the run" test -z "$(git -C "$WORK/repo" status --porcelain)"
exit $fail
