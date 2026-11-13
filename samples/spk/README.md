# .spk build output

This directory holds the `.spk` packages produced by `tools/build_spk.mjs`
(and their unpacked staging directories). Generated packages are ignored by
git; this README is kept.

```sh
node tools/gen_sample_key.mjs   # once; writes samples/keys/sample-2026.*
node tools/build_spk.mjs        # writes samples/spk/wireguard-1.0.0-<arch>.spk
```

The plugin sources live in `samples/wireguard` and `samples/realtime`. To try a
package locally, put the matching public key in `<config>/trust.d/<keyId>.pub`
and install the `.spk` through `plugin install` or the web console.

The legacy `samples/out/` directory is kept ignored for older builds.
