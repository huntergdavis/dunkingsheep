// Entry-point binding smoke control (PR84 and PR86 lesson): a build cannot
// see a free identifier that was never imported, and module tests never
// execute the app entry point. This parses the REAL main.jsx and asserts
// every name the entry point CALLS that is exported by a local module is
// declared by an import of that module in the same file.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));

// The local modules main.jsx consumes, with the names they export.
const localExports = {
  'theme-registry.js': ['buildThemeRegistry', 'groupThemes', 'THEME_GROUP_ORDER'],
  'leader-line.js': ['leaderLine'],
};

const mainBundle = () => {
  const src = readFileSync(path.join(here, 'main.jsx'), 'utf8');
  const imports = new Map(); // module -> Set of imported names
  for (const m of src.matchAll(/import\s*\{([^}]+)\}\s*from\s*'([^']+)'/g)) {
    const names = m[1].split(',').map(part => part.replace(/ as .*$/, '').trim());
    const list = imports.get(m[2]) ?? new Set();
    names.filter(Boolean).forEach(name => list.add(name));
    imports.set(m[2], list);
  }
  return { src, imports };
};

test('main.jsx binds every local export it calls to an import of that module', () => {
  const { src, imports } = mainBundle();
  const unbound = [];
  for (const [module, exports] of Object.entries(localExports)) {
    const imported = imports.get('./' + module) ?? new Set();
    for (const name of exports) {
      const used = new RegExp('\\b' + name + '\\s*\\(').test(src);
      if (used && !imported.has(name)) unbound.push(name + ' from ' + module);
    }
  }
  assert.deepEqual(unbound, [], 'referenced local exports not imported: ' + unbound.join(', '));
});

test('main.jsx does not import local names it never calls', () => {
  const { src, imports } = mainBundle();
  for (const [module, exports] of Object.entries(localExports)) {
    const imported = imports.get('./' + module) ?? new Set();
    for (const name of exports) {
      if (!imported.has(name)) continue;
      const used = new RegExp('\\b' + name + '\\s*\\(').test(src);
      assert.ok(used || name === 'THEME_GROUP_ORDER',
                name + ' imported from ' + module + ' but never called');
    }
  }
});
