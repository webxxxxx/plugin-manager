# .spk build output

This directory holds the `.spk` packages produced by `tools/build_spk.mjs`
(and their unpacked staging directories). Generated packages are ignored by
git; this README is kept.

The sample source lives in `samples/wireguard/` (manifest without `files[]`,
plus payload/config/hooks). Its manifest is `schema: 2` and declares
`interpreter: {"name": "python3", "version": ">=3.8"}` because the payload is
a Python script.

```sh
node tools/gen_sample_key.mjs          # once; writes samples/keys/sample-2026.*
node tools/build_spk.mjs wireguard     # writes samples/spk/wireguard-1.0.0-<arch>.spk
node tools/check_spk.mjs samples/spk/wireguard-1.0.0-x86_64.spk
```

The sample targets `x86_64-unknown-linux-musl`; the package will be rejected on
other platforms by the normal platform check.

To try a package locally, trust its signing key once (as Administrator):

```powershell
svc trust add samples\keys\sample-2026.pub
```

or upload `samples\keys\sample-2026.pub` with the web console's 上传公钥 button.
`svc trust list` shows the trusted ids and `svc trust remove sample-2026`
revokes one. The trust root is deliberately never inside the `.spk`.

Then install the `.spk` through `plugin install` or the web console.

The legacy `samples/out/` directory is kept ignored for older builds.
