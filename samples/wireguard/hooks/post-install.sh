#!/bin/sh
# Sample post-install hook. Runs with the installer's privileges.
set -eu
: "$SVCD_PLUGIN_ID"
: "$SVCD_DATA_DIR"
mkdir -p "$SVCD_DATA_DIR/state"
echo "wireguard: post-install ok for $SVCD_PLUGIN_ID" >&2
