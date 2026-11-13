#!/bin/sh
# Example svcd core service.
#
# svcd calls only the actions this file declares: `start` and `status`.
# There is no `stop` action, so svcd stops the service by terminating the
# process (SIGTERM to the process group); `status` stdout is shown as
# supplementary detail by `svc status`.
# Copy it into <config>/services.d/clock.sh and make it executable.
set -eu

action="${1:-${SVCD_ACTION:-start}}"

case "$action" in
  start)
    while :; do
      date '+%Y-%m-%d %H:%M:%S'
      sleep 2
    done
    ;;
  status)
    echo "clock: printing the time every two seconds"
    ;;
  *)
    exit 64
    ;;
esac
