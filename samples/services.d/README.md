# Core service script examples

These are drop-in `svcd` core services. svcd only calls the actions a script
declares:

- `start` — declared: svcd runs `<script> start`. Not declared: svcd runs the
  script from the top with no action argument.
- `stop` — declared: svcd runs `<script> stop` before stopping. Not declared:
  svcd terminates the process instead (SIGTERM on Unix; the Windows
  force-kill for an exec-ready script).
- `status` — optional detail only: `svc status` runs `<script> status` and
  shows its stdout as `details.probe`. It never changes the supervised state.

Actions are detected by name: shell functions (`start()` / `function start`,
or a `case` branch `start)`), PowerShell `function start`, and cmd labels
(`:start`, case-insensitive). Comments are ignored.

Every process a script starts stays in that service's process group (Unix) or
job object (Windows): stopping the service terminates the whole tree, and the
service stays `running` while any member of that tree is still alive. A missing
`stop` action therefore means the framework kills the tree for you.

`clock.sh` / `clock.bat` declare only `start` and `status`, so `svc stop`
terminates them. The file stem is the service id.

- `clock.sh` — Linux / macOS (POSIX sh)
- `clock.bat` — Windows

Both print the current time once every two seconds.

## Try one

Windows:

```powershell
Copy-Item samples/services.d/clock.bat `
  "$env:ProgramData\svcd\config\services.d\clock.bat"
Restart-Service svcd
svc list
svc logs clock
svc status clock
svc stop clock
```

Linux/macOS:

```sh
cp samples/services.d/clock.sh /etc/svcd/services.d/clock.sh
chmod +x /etc/svcd/services.d/clock.sh
# restart the configured svcd service (systemd, launchd, ...)
```

The configuration directory depends on the platform and on `svcd.conf`'s
`services_dir`. The daemon scans `services.d` only at startup, so restart
`svcd` after adding or removing a definition.

The service id must match `[a-z0-9][a-z0-9_-]{0,63}`; use the file name
(`clock.sh` / `clock.bat` -> `clock`).

## Package a drop-in script as a `.spk`

The same script can ship as a signed plugin without changing it. `--from-core`
reads the declared `start/stop/status` actions and writes a schema-2 manifest;
`--interpreter` is required because the payload is a script.

Windows:

```powershell
plugin bundle samples\services.d --exec clock.bat --from-core `
  --interpreter cmd --interpreter-arg /C `
  --key samples\keys\sample-2026.key --id clock --desc "clock service" `
  -o samples\spk\clock.spk
```

Linux/macOS:

```sh
plugin bundle samples/services.d --exec clock.sh --from-core --interpreter sh \
  --key samples/keys/sample-2026.key --id clock --desc "clock service" \
  -o samples/spk/clock.spk
```

Then trust `samples/keys/sample-2026.pub` once and install the package with
`plugin install`. See [../spk/README.md](../spk/README.md) for the key and
install steps.
