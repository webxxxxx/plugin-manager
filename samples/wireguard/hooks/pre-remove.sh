#!/bin/sh
# Sample pre-remove hook. A failure here must not block uninstall.
set -eu
echo "wireguard: pre-remove for $SVCD_PLUGIN_ID" >&2
