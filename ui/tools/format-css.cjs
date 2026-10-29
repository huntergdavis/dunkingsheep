const fs = require('fs');
const crypto = require('crypto');
const path = require('path');
const postcss = require('postcss');
const parserVersion = require('postcss/package.json').version;


function normalize(node) {
  switch (node.type) {
    case 'root': return {type:node.type,nodes:node.nodes.map(normalize)};
    case 'rule': return {type:node.type,selector:node.selector,nodes:node.nodes.map(normalize)};
    case 'atrule': return {type:node.type,name:node.name,params:node.params,nodes:node.nodes ? node.nodes.map(normalize) : null};
    case 'decl': return {type:node.type,prop:node.prop,value:node.value,important:!!node.important};
    case 'comment': return {type:node.type,text:node.text};
    default: throw Error('Unexpected node type: '+node.type);
  }
}
function formatCss(before) {
if (parserVersion !== '8.5.29') throw Error('Expected PostCSS 8.5.29, got '+parserVersion);
const root=postcss.parse(before);
root.walk(node => {
  const depth=(()=>{let n=node.parent,d=0;while(n&&n.type!=='root'){d++;n=n.parent;}return d;})();
  if (node.type==='decl') {
    node.raws.before=' ';
    node.raws.between=': ';
  } else {
    node.raws.before='\n'+'  '.repeat(depth);
    if (node.type==='rule') {node.raws.between=' ';node.raws.after=' ';node.raws.semicolon=true;}
    if (node.type==='atrule' && node.nodes) {node.raws.between=' ';node.raws.after='\n'+'  '.repeat(depth);node.raws.semicolon=true;}
  }
});
root.nodes[0].raws.before='';root.raws.after='\n';
const after=root.toString();
const normBefore=JSON.stringify(normalize(postcss.parse(before)));
const normAfter=JSON.stringify(normalize(postcss.parse(after)));
if(normBefore!==normAfter)throw Error('Formatting changed normalized CSS AST');
const negative=postcss.parse(after);let changed=false;
negative.walkDecls(node=>{if(!changed){node.value=node.value==='initial'?'inherit':'initial';changed=true;}});
const negativeDetected=normAfter!==JSON.stringify(normalize(negative));
if(!changed||!negativeDetected)throw Error('One-declaration negative control was not detected');

const sha=s=>crypto.createHash('sha256').update(s).digest('hex');
const evidence={parser:'postcss '+parserVersion,normalizedAstEqual:true,normalizedAstSha256:sha(normAfter),negativeOneDeclarationDetected:negativeDetected,beforeCssSha256:sha(before),afterCssSha256:sha(after),beforeLines:before.split('\n').length-1,afterLines:after.split('\n').length-1,comparison:'Ordered node types, selectors, declarations (property/value/important), at-rule names/params/structure, comments; raw whitespace and source positions excluded'};
return {after, evidence};
}

// Layout algorithm above is the PR40 contract, supplied by codex-design.
function firstDifference(actual, expected) {
  const a=actual.split('\n'), b=expected.split('\n');
  for(let i=0;i<Math.max(a.length,b.length);i++) if(a[i]!==b[i]) return i+1;
  return null;
}
function main(args) {
  const mode=args[0]?.startsWith('--') ? args.shift() : '--check';
  if(!['--check','--write'].includes(mode)||args.length>1)
    throw Error('Usage: node web/tools/format-css.cjs [--check|--write] [file.css]');
  const file=path.resolve(args[0]||path.join(__dirname,'../src/style.css'));
  const before=fs.readFileSync(file,'utf8');
  const {after,evidence}=formatCss(before);
  const line=firstDifference(before,after);
  if(mode==='--check' && line!==null)
    throw Object.assign(Error('MISMATCH '+file+':'+line),{exitCode:1});
  if(mode==='--write' && line!==null) {
    const temporary=file+'.format-'+crypto.randomUUID()+'.tmp';
    let fd;
    try {
      fd=fs.openSync(temporary,'wx',fs.statSync(file).mode & 0o777);
      fs.writeFileSync(fd,after);
      fs.fsyncSync(fd);
      fs.closeSync(fd);fd=undefined;
      fs.renameSync(temporary,file);
    } finally {
      if(fd!==undefined)fs.closeSync(fd);
      if(fs.existsSync(temporary))fs.unlinkSync(temporary);
    }
    console.error(file+': formatted (first changed line '+line+')');
  }
  console.log(JSON.stringify({...evidence,mode,byteIdentical:line===null},null,2));
}
module.exports={formatCss,firstDifference};
if(require.main===module) {
  try { main(process.argv.slice(2)); }
  catch(error) { console.error(error.exitCode===1 ? error.message : 'CSS formatter: '+error.message); process.exitCode=error.exitCode||2; }
}
