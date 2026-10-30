const fs = require('node:fs');
const path = require('node:path');
const postcss = require('postcss');

// Reuse the formatter's pinned parser; inspect every selector branch, including
// nested @media/@supports rules. Variant CSS must spell its own root constraint.
function checkThemeScope(css, id, file = id + '.css') {
  if (require('postcss/package.json').version !== '8.5.29')
    throw Error('Expected PostCSS 8.5.29');
  if (!/^[a-z0-9][a-z0-9-]*$/.test(id)) throw Error('Invalid theme id: ' + id);
  const root = postcss.parse(css, {from: file});
  if (id === 'default') return [];
  const ownRoot = new RegExp('^:root\\[data-theme\\s*=\\s*(?:"' + id +
    '"|\'' + id + '\'|' + id + ')\\s*\\](?=$|[\\s.#:\\[>+~])');
  const errors = [];
  const isKeyframes = name => /^(?:-[a-z]+-)?keyframes$/i.test(name);
  root.walkRules(rule => {
    // Keyframe steps are not DOM selectors; report their global name below.
    for (let parent = rule.parent; parent; parent = parent.parent) {
      if (parent.type === 'atrule' && isKeyframes(parent.name)) return;
    }
    for (const selector of rule.selectors) {
      if (!ownRoot.test(selector.trim()))
        errors.push(file + ':' + rule.source.start.line + ': unscoped selector: ' + selector);
    }
  });
  root.walkAtRules(rule => {
    const name = rule.name.toLowerCase();
    if (name === 'import')
      errors.push(file + ':' + rule.source.start.line + ': variant @import bypasses scope checks');
    if (name === 'font-face' || name === 'property' || isKeyframes(name))
      errors.push(file + ':' + rule.source.start.line + ': variant @' + name +
        ' registers a global name outside theme selector scope');
  });
  return errors;
}

function checkDirectory(directory) {
  const files = fs.readdirSync(directory).filter(file => file.endsWith('.css')).sort();
  if (!files.length) throw Error('No theme stylesheets in ' + directory);
  return files.flatMap(file => checkThemeScope(
    fs.readFileSync(path.join(directory, file), 'utf8'), file.slice(0, -4), path.join(directory, file)));
}

module.exports = {checkThemeScope, checkDirectory};
if (require.main === module) {
  try {
    const errors = checkDirectory(process.argv[2] || path.join(__dirname, '../src/themes'));
    if (errors.length) {
      console.error(errors.join('\n'));
      process.exitCode = 1;
    } else console.log('Theme selector scope: PASS');
  } catch (error) {
    console.error(error.message);
    process.exitCode = 2;
  }
}
