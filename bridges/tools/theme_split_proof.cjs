const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const postcss = require(path.resolve(__dirname, '../web/node_modules/postcss'));
const [beforePath, afterPath] = process.argv.slice(2);
if (!beforePath || !afterPath) throw Error('Usage: node tools/theme_split_proof.cjs BEFORE.css AFTER.css');
const before = fs.readFileSync(beforePath, 'utf8'), after = fs.readFileSync(afterPath, 'utf8');
function normalize(node) {
  const keys = {rule: ['selector'], decl: ['prop', 'value', 'important'], atrule: ['name', 'params']};
  return {type: node.type, ...Object.fromEntries((keys[node.type] || []).map(key => [key, node[key] || false])),
    ...(node.nodes ? {nodes: node.nodes.filter(n => n.type !== 'comment').map(normalize)} : {})};
}
const parsedBefore = postcss.parse(before), parsedAfter = postcss.parse(after);
const a = normalize(parsedBefore), b = normalize(parsedAfter);
const orderedAstEqual = JSON.stringify(a) === JSON.stringify(b);
const canonicalRules = root => JSON.stringify(root.nodes.map(node => JSON.stringify(node)).sort());
const ruleForRuleEqualIgnoringTopLevelOrder = canonicalRules(a) === canonicalRules(b);
const negative = parsedAfter.clone();
let changed = false;
negative.walkDecls(node => {if (!changed) {node.value = node.value === 'initial' ? 'inherit' : 'initial'; changed = true;}});
const negativeDetected = changed && canonicalRules(b) !== canonicalRules(normalize(negative));
const sha = text => crypto.createHash('sha256').update(text).digest('hex');
const evidence = {beforePath, afterPath, beforeSha256: sha(before), afterSha256: sha(after),
  emittedCssByteIdentical: before === after, orderedAstEqual,
  ruleForRuleEqualIgnoringTopLevelOrder, topLevelRulesBefore: a.nodes.length,
  topLevelRulesAfter: b.nodes.length, negativeOneDeclarationDetected: negativeDetected,
  comparison: 'Whitespace/comments ignored; selectors and ordered declarations/at-rule children preserved. Top-level order ignored ONLY by the explicitly named rule-for-rule comparison. Browser computed-style equality must be checked separately.'};
console.log(JSON.stringify(evidence, null, 2));
if (!(before === after || ruleForRuleEqualIgnoringTopLevelOrder) || !negativeDetected) process.exitCode = 1;
