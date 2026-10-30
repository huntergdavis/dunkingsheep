const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {checkThemeScope, checkDirectory} = require('./check-theme-scope.cjs');

test('repository variants remain constrained to their own theme', () => {
  assert.deepEqual(checkDirectory(path.join(__dirname, '../src/themes')), []);
});

test('default base is exempt; numeric IDs and scoped selector lists work', () => {
  assert.deepEqual(checkThemeScope(':root { --ink: black } body { color: black }', 'default'), []);
  assert.deepEqual(checkThemeScope(`
    :root[data-theme="2001"] { --ink: black }
    @media print {
      :root[data-theme='2001'] body::after,
      :root[data-theme=2001] :is(h1, h2, .brand) { display: none }
    }
  `, '2001'), []);
});

test('unscoped selector branch fails even beside a scoped branch', () => {
  const errors = checkThemeScope(':root[data-theme="nostromo"] h1, body { color: red }', 'nostromo');
  assert.equal(errors.length, 1);
  assert.match(errors[0], /unscoped selector: body$/);
});

test('nested conditional leaks and another theme root fail', () => {
  const errors = checkThemeScope(`
    @media print { @supports (display: grid) { body { color: red } } }
    :root[data-theme="crt"] { --ink: red }
    :root { --surface: red }
  `, 'nostromo');
  assert.equal(errors.length, 3);
});

test('a textual prefix inside a different theme ID does not grant scope', () => {
  assert.equal(checkThemeScope(':root[data-theme="crt-extra"] { color: red }', 'crt').length, 1);
});

test('variant imports cannot introduce unchecked global selectors', () => {
  assert.match(checkThemeScope('@import "global.css";', 'crt')[0], /bypasses scope checks/);
});

test('global font, property and keyframe registrations have explicit errors', () => {
  for (const [name, css] of [
    ['font-face', '@font-face { font-family: Shared; src: local(Arial) }'],
    ['property', '@PROPERTY --ink { syntax: "<color>"; inherits: true; initial-value: red }'],
    ['keyframes', '@keyframes shared { from { opacity: 0 } to { opacity: 1 } }'],
    ['-webkit-keyframes', '@-webkit-keyframes shared { 0% { opacity: 0 } 100% { opacity: 1 } }'],
  ]) {
    const errors = checkThemeScope(css, 'crt');
    assert.equal(errors.length, 1, css);
    assert.ok(errors[0].includes('variant @' + name + ' registers a global name'), errors[0]);
    assert.ok(!errors[0].includes('unscoped selector'), errors[0]);
  }
  assert.equal(checkThemeScope('@IMPORT "global.css";', 'crt').length, 1);
});

test('wrapped root constraints and native nesting are conservatively rejected', () => {
  for (const selector of [':is(:root[data-theme="crt"])', ':where(:root[data-theme="crt"])']) {
    assert.equal(checkThemeScope(selector + ' .x { color: red }', 'crt').length, 1);
  }
  assert.equal(checkThemeScope(':root[data-theme="crt"] { .nested { color: red } }', 'crt').length, 1);
});
