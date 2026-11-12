#!/usr/bin/env node
// Generate the sample Ed25519 keypair (test-only). Run once; use --force to rotate.
import { generateKeyPairSync } from 'node:crypto';
import { writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const KEYS = join(ROOT, 'samples', 'keys');
const priv = join(KEYS, 'sample-2026.key');
const pub = join(KEYS, 'sample-2026.pub');

if (existsSync(priv) && !process.argv.includes('--force')) {
  console.log('keypair exists: ' + priv + ' (use --force to rotate)');
  process.exit(0);
}
mkdirSync(KEYS, { recursive: true });
const kp = generateKeyPairSync('ed25519', {
  publicKeyEncoding: { type: 'spki', format: 'pem' },
  privateKeyEncoding: { type: 'pkcs8', format: 'pem' }
});
writeFileSync(priv, kp.privateKey);
writeFileSync(pub, kp.publicKey);
console.log('wrote ' + priv);
console.log('wrote ' + pub);
