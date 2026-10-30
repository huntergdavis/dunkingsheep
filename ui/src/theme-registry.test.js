import test from 'node:test';
import assert from 'node:assert/strict';
import { buildThemeRegistry, groupThemes } from './theme-registry.js';

const manifest = (id, name, order) => [ './themes/'+id+'.json', { id, name, order } ];
const registry = rows => buildThemeRegistry(Object.fromEntries(rows),
  Object.fromEntries(rows.map(([file]) => [file.replace('.json', '.css'), {}])));

test('two new files add a theme without changing a shared registry', () => {
  const rows = [manifest('crt', '1980s CRT', 10), manifest('default', 'Default', 100)];
  assert.deepEqual(registry(rows).map(t => t.id), ['default', 'crt']);
  rows.push(manifest('future', 'Future', 5));
  assert.deepEqual(registry(rows).map(t => t.id), ['default', 'future', 'crt']);
});
test('default first, then numeric order, name and stable id tie-break', () => {
  const rows = [manifest('z', 'Same', 2), manifest('b', 'Beta', 1),
    manifest('a', 'Alpha', 1), manifest('default', 'Default', 99), manifest('x', 'Same', 2)];
  assert.deepEqual(registry(rows).map(t => t.id), ['default', 'a', 'b', 'x', 'z']);
});
test('group passes through when present and is absent otherwise', () => {
  const rows = [manifest('default', 'Default', 0), manifest('crt', '1980s CRT', 10)];
  Object.assign(Object.fromEntries(rows)['./themes/crt.json'], { group: ' Classic sci-fi ' });
  const themes = registry(rows);
  assert.equal(themes[0].group, undefined);
  assert.equal(themes[1].group, 'Classic sci-fi');
});
test('an empty or non-string group fails visibly', () => {
  for (const group of ['', 7]) {
    const rows = [manifest('default', 'Default', 0), manifest('crt', '1980s CRT', 10)];
    Object.assign(Object.fromEntries(rows)['./themes/crt.json'], { group });
    assert.throws(() => registry(rows), /Invalid theme manifest/);
  }
});
test('interleaved group orders still render two groups, pinned order, nothing lost', () => {
  const rows = [manifest('default', 'Default', 0), manifest('crt', '1980s CRT', 10),
    manifest('a', 'Alpha', 20), manifest('b', 'Beta', 30)];
  Object.assign(Object.fromEntries(rows)['./themes/crt.json'], { group: 'Standard' });
  Object.assign(Object.fromEntries(rows)['./themes/default.json'], { group: 'Standard' });
  Object.assign(Object.fromEntries(rows)['./themes/a.json'], { group: 'Classic sci-fi' });
  Object.assign(Object.fromEntries(rows)['./themes/b.json'], { group: 'Standard', order: 120 });
  const groups = groupThemes(buildThemeRegistry(Object.fromEntries(rows),
    Object.fromEntries(rows.map(([file]) => [file.replace('.json', '.css'), {}]))));
  assert.deepEqual(groups.map(g => g.label), ['Standard', 'Classic sci-fi']);
  assert.deepEqual(groups[0].options.map(t => t.id), ['default', 'crt', 'b']);
  assert.deepEqual(groups[1].options.map(t => t.id), ['a']);
  assert.equal(groups.flatMap(g => g.options).length, 4);
});
test('an ungrouped theme lands in Other without splitting or duplicating a label', () => {
  const rows = [manifest('default', 'Default', 0), manifest('crt', '1980s CRT', 10),
    manifest('a', 'Alpha', 20), manifest('z', 'Zulu', 30)];
  Object.assign(Object.fromEntries(rows)['./themes/crt.json'], { group: 'Standard' });
  Object.assign(Object.fromEntries(rows)['./themes/a.json'], { group: 'Classic sci-fi' });
  Object.assign(Object.fromEntries(rows)['./themes/z.json'], { group: 'Classic sci-fi' });
  const groups = groupThemes(buildThemeRegistry(Object.fromEntries(rows),
    Object.fromEntries(rows.map(([file]) => [file.replace('.json', '.css'), {}]))));
  assert.deepEqual(groups.map(g => g.label), ['Standard', 'Classic sci-fi', 'Other']);
  assert.deepEqual(groups[2].options.map(t => t.id), ['default']);
});
test('a label outside the pinned order sorts after it, alphabetically', () => {
  const rows = [manifest('default', 'Default', 0), manifest('x', 'X-ray', 10), manifest('m', 'Mike', 20)];
  Object.assign(Object.fromEntries(rows)['./themes/x.json'], { group: 'Zulu' });
  Object.assign(Object.fromEntries(rows)['./themes/m.json'], { group: 'Alpha' });
  const groups = groupThemes(buildThemeRegistry(Object.fromEntries(rows),
    Object.fromEntries(rows.map(([file]) => [file.replace('.json', '.css'), {}]))), []);
  assert.deepEqual(groups.map(g => g.label), ['Alpha', 'Other', 'Zulu']);
});
test('invalid or incomplete theme pairs fail visibly', () => {
  const base = Object.fromEntries([manifest('default', 'Default', 0)]);
  assert.throws(() => buildThemeRegistry(base, {}), /stylesheet missing/);
  assert.throws(() => buildThemeRegistry({}, {'./themes/default.css': {}}), /manifest missing/);
  assert.throws(() => registry([manifest('crt', 'CRT', 1)]), /Default theme/);
  for (const value of [{id:'other',name:'Wrong',order:1}, {id:'default',name:'',order:0},
    {id:'default',name:'Default',order:'1'}]) {
    assert.throws(() => buildThemeRegistry({'./themes/default.json':value},
      {'./themes/default.css':{}}), /Invalid theme manifest/);
  }
});
