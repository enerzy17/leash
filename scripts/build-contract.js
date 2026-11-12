// Build the contract and write res/leash.wasm.
//   npm run build:contract
// Needs Rust (the pinned toolchain in rust-toolchain.toml installs itself through rustup).
// Mirrors what cargo-near does: release build with `--cfg near`, then wasm-opt for size.

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, statSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const built = join(root, 'target', 'wasm32-unknown-unknown', 'release', 'leash_contract.wasm');
const out = join(root, 'res', 'leash.wasm');
const run = (cmd, args) => execFileSync(cmd, args, { cwd: root, stdio: 'inherit' });

run('cargo', ['build', '-p', 'leash-contract', '--target', 'wasm32-unknown-unknown', '--release', '--locked']);

const require = createRequire(import.meta.url);
const wasmOpt = join(dirname(require.resolve('binaryen/package.json')), 'bin', 'wasm-opt');
mkdirSync(dirname(out), { recursive: true });
run(process.execPath, [wasmOpt, '-Os', '--enable-bulk-memory', '--enable-nontrapping-float-to-int', built, '-o', out]);

const bytes = readFileSync(out);
console.log(`res/leash.wasm  ${statSync(out).size} bytes  sha256 ${createHash('sha256').update(bytes).digest('hex')}`);
