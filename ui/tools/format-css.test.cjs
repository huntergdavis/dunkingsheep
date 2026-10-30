const test=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const os=require('node:os');
const path=require('node:path');
const {spawnSync}=require('node:child_process');
const {formatCss,firstDifference}=require('./format-css.cjs');
const script=path.join(__dirname,'format-css.cjs');
test('check rejects a formatting difference without writing, and write is explicit',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'vp-css-format-'));
  try {
    const file=path.join(dir,'fixture.css'),source='a{color:red}\n';
    fs.writeFileSync(file,source);
    const check=spawnSync(process.execPath,[script,'--check',file],{cwd:dir,encoding:'utf8'});
    assert.equal(check.status,1);
    assert.match(check.stderr,/^MISMATCH .+:1\n$/);
    assert.equal(fs.readFileSync(file,'utf8'),source);
    const write=spawnSync(process.execPath,[script,'--write',file],{cwd:dir,encoding:'utf8'});
    assert.equal(write.status,0,write.stderr);
    assert.equal(fs.readFileSync(file,'utf8'),'a { color: red; }\n');
    const second=spawnSync(process.execPath,[script,'--check',file],{cwd:dir,encoding:'utf8'});
    assert.equal(second.status,0,second.stderr);
    assert.equal(JSON.parse(second.stdout).byteIdentical,true);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
test('ordered CSS structure survives formatting and a declaration mutation is detected',()=>{
  const input='/* note */@media(min-width:1px){a,b{color:red!important;--x:2}}';
  const {after,evidence}=formatCss(input);
  assert.equal(evidence.normalizedAstEqual,true);
  assert.equal(evidence.negativeOneDeclarationDetected,true);
  assert.equal(formatCss(after).after,after);
  assert.notEqual(formatCss(after.replace('red','blue')).evidence.normalizedAstSha256,evidence.normalizedAstSha256);
  assert.equal(firstDifference(after,after.replace('red','blue')),3);
});
test('malformed CSS fails without replacing source',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'vp-css-invalid-'));
  try {
    const file=path.join(dir,'fixture.css'),source='a { color: red;';
    fs.writeFileSync(file,source);
    const result=spawnSync(process.execPath,[script,'--write',file],{encoding:'utf8'});
    assert.equal(result.status,2);
    assert.match(result.stderr,/CSS formatter:/);
    assert.equal(fs.readFileSync(file,'utf8'),source);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
test('no mode defaults to a read-only check, including no arguments',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'vp-css-default-'));
  try {
    fs.mkdirSync(path.join(dir,'tools'));fs.mkdirSync(path.join(dir,'src'));
    fs.copyFileSync(script,path.join(dir,'tools/format-css.cjs'));
    fs.symlinkSync(path.resolve(__dirname,'../node_modules'),path.join(dir,'node_modules'),'dir');
    const file=path.join(dir,'src/style.css'),source='a{color:red}'+String.fromCharCode(10);
    fs.writeFileSync(file,source);
    for(const args of [[],[file]]) {
      const result=spawnSync(process.execPath,[path.join(dir,'tools/format-css.cjs'),...args],{cwd:dir,encoding:'utf8'});
      assert.equal(result.status,1);
      assert.match(result.stderr,/^MISMATCH .+:1\n$/);
      assert.equal(fs.readFileSync(file,'utf8'),source);
    }
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
test('check mismatch and usage have stable, distinct exit codes',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'vp-css-exits-'));
  try {
    const file=path.join(dir,'fixture.css'),source='a { color: red; }\nb{color:blue}\n';
    fs.writeFileSync(file,source);
    const mismatch=spawnSync(process.execPath,[script,'--check',file],{encoding:'utf8'});
    assert.equal(mismatch.status,1);
    assert.equal(mismatch.stderr,'MISMATCH '+file+':2\n');
    const usage=spawnSync(process.execPath,[script,'--unknown',file],{encoding:'utf8'});
    assert.equal(usage.status,2);
    assert.match(usage.stderr,/Usage:/);
    assert.equal(fs.readFileSync(file,'utf8'),source);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
test('write safety failures and failed atomic rename leave original bytes intact',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'vp-css-atomic-'));
  try {
    const file=path.join(dir,'fixture.css');
    const run=preload=>spawnSync(process.execPath,[...(preload?['--require',preload]:[]),script,'--write',file],{encoding:'utf8'});
    fs.writeFileSync(file,'/* no declaration for negative control */\n');
    let before=fs.readFileSync(file,'utf8');
    let result=run();
    assert.equal(result.status,2);
    assert.match(result.stderr,/negative control was not detected/);
    assert.equal(fs.readFileSync(file,'utf8'),before);
    fs.writeFileSync(file,'a{color:red}\n');before=fs.readFileSync(file,'utf8');
    const preload=path.join(dir,'fault.cjs');
    // Mutate the second parser result: the same safety comparator must catch it.
    fs.writeFileSync(preload,"const p=require("+JSON.stringify(require.resolve('postcss'))+");const parse=p.parse;let n=0;p.parse=(...args)=>{const root=parse(...args);if(++n===2)root.walkDecls(d=>d.value='initial');return root;};");
    result=run(preload);
    assert.equal(result.status,2);
    assert.match(result.stderr,/changed normalized CSS AST/);
    assert.equal(fs.readFileSync(file,'utf8'),before);
    // A failed replacement must preserve the original and clean up the temp file.
    fs.writeFileSync(preload,"require('fs').renameSync=()=>{throw Error('injected rename failure')};");
    result=run(preload);
    assert.equal(result.status,2);
    assert.match(result.stderr,/injected rename failure/);
    assert.equal(fs.readFileSync(file,'utf8'),before);
    assert.equal(fs.readdirSync(dir).filter(name=>name.includes('.format-')).length,0);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
test('wrong parser version exits 2 with one line and no file changes',()=>{
  const dir=fs.mkdtempSync(path.join(os.tmpdir(),'vp-css-version-'));
  try {
    fs.mkdirSync(path.join(dir,'tools'));
    fs.mkdirSync(path.join(dir,'node_modules/postcss'),{recursive:true});
    fs.copyFileSync(script,path.join(dir,'tools/format-css.cjs'));
    // A local package fixture resolves the installed parser with incompatible version metadata.
    fs.writeFileSync(path.join(dir,'node_modules/postcss/package.json'),JSON.stringify({name:'postcss',version:'8.5.8',main:'index.cjs'}));
    fs.writeFileSync(path.join(dir,'node_modules/postcss/index.cjs'),'module.exports=require('+JSON.stringify(require.resolve('postcss'))+');');
    const file=path.join(dir,'fixture.css'),source='a{color:red}\n';
    fs.writeFileSync(file,source);
    const result=spawnSync(process.execPath,[path.join(dir,'tools/format-css.cjs'),'--write',file],{cwd:dir,encoding:'utf8'});
    assert.equal(result.status,2);
    assert.equal(result.stderr,'CSS formatter: Expected PostCSS 8.5.29, got 8.5.8\n');
    assert.equal(result.stdout,'');
    assert.equal(fs.readFileSync(file,'utf8'),source);
    assert.equal(fs.readdirSync(dir).filter(name=>name.includes('.format-')).length,0);
  } finally {fs.rmSync(dir,{recursive:true,force:true});}
});
