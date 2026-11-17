import { verifySource } from './source.mjs';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
const root=dirname(fileURLToPath(import.meta.url));
const repo=resolve(root,'../..');
const pin=verifySource(root);
const fingerprint=createHash('sha256').update(pin).update(readFileSync(resolve(root,'build.mjs'))).update(readFileSync(resolve(root,'security.patch'))).digest('hex');
const stamp=resolve(repo,'ui/public/webmail/.ds-build');
if (!existsSync(stamp) || readFileSync(stamp,'utf8')!==fingerprint) {
  execFileSync(process.execPath,[resolve(root,'build.mjs')],{cwd:repo,stdio:'inherit'});
  writeFileSync(stamp,fingerprint);
}
