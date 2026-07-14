#!/usr/bin/env bash
# Restore the tuning fixtures to /tmp exactly as the drive-vet loop expects
# (LOG.md references /tmp paths). Each fixture becomes a git repo with a
# single "baseline" commit; drive.sh and vet.sh land in /tmp.
#
# Usage: docs/tuning/fixtures/restore.sh
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)

for f in f4-analysis f5-hard-algo f6-multifile-bug f7-hard-algo-2; do
  rm -rf "/tmp/$f"
  cp -r "$HERE/$f" "/tmp/$f"
  git -C "/tmp/$f" init -q
  git -C "/tmp/$f" config user.name fixture
  git -C "/tmp/$f" config user.email fixture@local
  git -C "/tmp/$f" add -A
  git -C "/tmp/$f" commit -qm baseline
  echo "restored /tmp/$f"
done

cp "$HERE/drive.sh" "$HERE/vet.sh" /tmp/
chmod +x /tmp/drive.sh /tmp/vet.sh
mkdir -p /tmp/fixture-logs
echo "drive.sh + vet.sh installed; run e.g.: XDG_DATA_HOME=/tmp/xdg-a /tmp/drive.sh f5-hard-algo TAG 5"
