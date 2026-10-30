// Use the same pinned parser as the formatter; never mutate the parsed files.
const fs = require('fs');
const postcss = require('postcss');
const version = require('postcss/package.json').version;
try {
  if (version !== '8.5.29') throw Error('Expected PostCSS 8.5.29, got ' + version);
  const sources = JSON.parse(fs.readFileSync(0, 'utf8'));
  const rules = [];
  for (const {source, css} of sources) {
    const root = postcss.parse(css, {from: source});
    root.walkRules(rule => {
      const context = [];
      for (let p = rule.parent; p && p.type !== 'root'; p = p.parent)
        if (p.type === 'atrule') context.unshift('@' + p.name + ' ' + p.params);
      rules.push({source, line: rule.source.start.line, selector: rule.selector,
        context, declarations: rule.nodes.filter(n => n.type === 'decl').map(n =>
          ({property: n.prop, value: n.value, important: !!n.important}))});
    });
  }
  process.stdout.write(JSON.stringify({parser: 'postcss ' + version, rules}));
} catch (error) {
  process.stderr.write('Contrast consumers: ' + error.message + '\n');
  process.exitCode = 2;
}
