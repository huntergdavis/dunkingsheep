// Generated Bulwark assets from older builds must not enter new releases.
import { rmSync } from 'node:fs';
rmSync(new URL('../public/webmail/', import.meta.url), { recursive: true, force: true });
