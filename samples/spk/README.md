# .spk build output

This directory holds the `.spk` packages produced by `tools/build_spk.mjs`
(and their unpacked staging directories). Generated packages are ignored by
git; this README is kept.

```sh
node tools/gen_sample_key.mjs   # once; writes samples/keys/sample-2026.*
node tools/build_spk.mjs        # writes samples/spk/wireguard-1.0.0-<arch>.spk
```

To try a package locally, trust its signing key once (as Administrator):

```powershell
svc trust add samples\keys\sample-2026.pub
```

or upload `samples\keys\sample-2026.pub` in the web console's 安装插件 card.
`svc trust list` shows the trusted ids and `svc trust remove sample-2026`
revokes one. The trust root is deliberately never inside the `.spk`.

Then install the `.spk` through `plugin install` or the web console.

The legacy `samples/out/` directory is kept ignored for older builds.
