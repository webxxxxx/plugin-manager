# .spk build output

This directory is the single output location for generated `.spk` files
(plus build staging/extract directories, which are ignored by git). Sources
live under `samples/<name>/`; this README is kept in git, generated packages
are not.

- `samples/wireguard/` — manifest without `files[]`, payload/config/hooks.
  It is `schema: 2` and declares
  `interpreter: {"name": "python3", "version": ">=3.8"}` because the payload
  is a Python script. The package targets `x86_64-unknown-linux-musl`.
- `samples/clock/` — a Windows `.cmd` service used by both the core drop-in
  README and the schema-2 plugin sample. Its `lifecycle.stop` is generated from
  the script's declared `:stop` action.

```sh
node tools/gen_sample_key.mjs          # once; writes samples/keys/sample-2026.*
node tools/build_spk.mjs wireguard     # writes samples/spk/wireguard-1.0.0-<arch>.spk
node tools/check_spk.mjs samples/spk/wireguard-1.0.0-x86_64.spk
```

Windows clock sample:

```powershell
plugin bundle samples\clock --from-core --interpreter cmd --interpreter-arg /C `
  --key samples\keys\sample-2026.key --version 0.1.0 `
  -o samples\spk\clock-0.1.0-x86_64.spk
```

The wireguard sample targets `x86_64-unknown-linux-musl`; the package will be
rejected on other platforms by the normal platform check.

To try a package locally, trust its signing key once (as Administrator):

```powershell
svc trust add samples\keys\sample-2026.pub
```

or upload `samples\keys\sample-2026.pub` with the web console's 上传公钥 button.
`svc trust list` shows the trusted ids and `svc trust remove sample-2026`
revokes one. The trust root is deliberately never inside the `.spk`.

Then install the `.spk` through `plugin install` or the web console.

The legacy `samples/out/` directory is kept ignored for older builds.
