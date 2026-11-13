#!/bin/sh
# Example svcd core service.
#
# The daemon invokes this same file with `start`, `stop` or `status`.
# Copy it into <config>/services.d/clock.sh and make it executable.
set -eu

action="${1:-${SVCD_ACTION:-start}}"
dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
name=$(basename "$0" .sh)
stop_file="$dir/$name.stop"

case "$action" in
  start)
    rm -f "$stop_file"
    while [ ! -f "$stop_file" ]; do
      date '+%Y-%m-%d %H:%M:%S'
      sleep 2
    done
    rm -f "$stop_file"
    ;;
  stop)
    : > "$stop_file"
    ;;
  status)
    exit 0
    ;;
  *)
    exit 64
    ;;
esac
