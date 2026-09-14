#!/usr/bin/env bash
#
# Sign and publish a daemon build to the mesh.
#
#   scripts/publish-release.sh <binary> <version> [console-url] [group]
#
# The daemon signs the release with this node's identity key, stores the binary
# as content-addressed chunks, and writes a release@1 document that replicates
# like any other. Other nodes see an offer; nothing is applied anywhere unless
# the operator has already trusted this key -- and then only if update.auto is
# on, or they click Update.
#
# The platform is taken from the publishing daemon's own build target, so a
# release cannot claim to be for a target it was not built for.
set -euo pipefail

bin="${1:?usage: publish-release.sh <binary> <version> [console-url] [group]}"
version="${2:?a version is required, e.g. 1.9.0}"
console="${3:-http://127.0.0.1:4002}"
group="${4:-}"

[ -f "$bin" ] || { echo "no such binary: $bin" >&2; exit 1; }

# Refuse to publish a binary that does not agree with the version being
# claimed. A release whose version is a lie is worse than no release.
if reported="$("$bin" --version 2>/dev/null | awk '{print $2}')"; then
  if [ -n "$reported" ] && [ "$reported" != "$version" ]; then
    echo "refusing: $bin reports $reported but you are publishing it as $version" >&2
    exit 1
  fi
fi

echo "publishing $version ($(wc -c < "$bin") bytes) to $console"
curl -fsS -X POST \
  "$console/api/releases?version=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' "$version")${group:+&group=$group}" \
  -H 'Content-Type: application/octet-stream' \
  --data-binary "@$bin"
echo
