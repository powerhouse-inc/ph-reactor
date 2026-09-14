#!/usr/bin/env bash
#
# Build a plugin package and publish it to the local reactor.
#
#   scripts/publish-package.sh packages/achra [console-url] [group]
#
# "Build" is two steps, because a bundle is deliberately a single self-contained
# HTML document -- no archive, so no paths inside it, so no path traversal and
# no extraction step:
#
#   1. inline the SDK where the editor asks for it;
#   2. inline each document model's definition into the manifest.
#
# The daemon does the rest: it signs the manifest with this node's identity key,
# stores the editor as content-addressed chunks, and writes a package@1 document
# that replicates to everyone on the drive. Other nodes see an offer; nothing is
# installed anywhere until an operator says yes to the publisher key.
set -euo pipefail

dir="${1:?usage: publish-package.sh <package-dir> [console-url] [group]}"
console="${2:-http://127.0.0.1:4002}"
# Optional: put the package in a group's drive, so it is published BY that
# group rather than as a loose document. Attribution and placement only --
# every document replicates to every drive peer regardless of group.
group="${3:-}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$dir/powerhouse.manifest.json"

[ -f "$manifest" ] || { echo "no powerhouse.manifest.json in $dir" >&2; exit 1; }

body="$(python3 - "$dir" "$repo" "$group" <<'PY'
import json, os, sys

pkg_dir, repo = sys.argv[1], sys.argv[2]
group = sys.argv[3] if len(sys.argv) > 3 else ""
man = json.load(open(os.path.join(pkg_dir, "powerhouse.manifest.json")))

# -- the editor, with the SDK inlined --------------------------------------
editor = None
if man.get("editor"):
    html = open(os.path.join(pkg_dir, man["editor"]), encoding="utf-8").read()
    sdk = open(os.path.join(repo, "packages/sdk/ph-reactor-sdk.js"), encoding="utf-8").read()
    # The inlined copy is not a module being imported, so the export keywords
    # would be dead syntax. Strip them rather than rely on them being ignored.
    sdk = sdk.replace("export function ", "function ").replace("export default createClient;", "")
    marker = "<!--#include ph-reactor-sdk.js-->"
    if marker not in html:
        sys.exit(f"{man['editor']} has no {marker} placeholder")
    editor = html.replace(marker, sdk)

# -- the models, by value not by reference ---------------------------------
models = []
for ref in man.get("documentModels", []):
    loaded = json.load(open(os.path.join(pkg_dir, ref), encoding="utf-8"))
    # A model file may hold one definition or a {"models": [...]} collection.
    models.extend(loaded["models"] if isinstance(loaded, dict) and "models" in loaded else [loaded])

print(json.dumps({
    "name": man["name"],
    "version": man["version"],
    "description": man.get("description", ""),
    "category": man.get("category", ""),
    "publisher_name": man.get("publisher", {}).get("name", ""),
    "publisher_url": man.get("publisher", {}).get("url", ""),
    "document_models": models,
    "processors": man.get("processors", []),
    "capabilities": man.get("capabilities", {"read": [], "write": []}),
    "ui": man.get("ui", {"nav": []}),
    "group": group or None,
    "editor": editor,
}))
PY
)"

echo "publishing $(python3 -c 'import json,sys;m=json.load(open(sys.argv[1]));print(m["name"], m["version"])' "$manifest") to $console"
curl -fsS -X POST "$console/api/packages" \
  -H 'Content-Type: application/json' \
  --data-binary "$body"
echo
