# Core service script examples

These are drop-in `svcd` core services. The daemon calls the same script with
`start`, `stop` and `status`, and the file stem is the service id.

- `clock.sh` — Linux / macOS (POSIX sh)
- `clock.bat` — Windows

Both print the current time once every two seconds and stop when the daemon
invokes the `stop` action (the example writes a sidecar `.stop` file; the loader
ignores that extension).

## Try one

Windows:

```powershell
Copy-Item samples/services.d/clock.bat `
  "$env:ProgramData\svcd\config\services.d\clock.bat"
Restart-Service svcd
svc list
svc logs clock
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

## Plugin packages

See `../spk/` for the `.spk` build output directory and
`samples/wireguard` / `samples/realtime` for plugin sources.
