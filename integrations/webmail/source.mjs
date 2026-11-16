import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';

// The parent gitlink is authoritative, including staged pin updates.
export function verifySource(root) {
  const repo=resolve(root,'../..');
  const source=resolve(root,'bulwark');
  const git=(cwd,args)=>execFileSync('git',args,{cwd,encoding:'utf8'}).trim();
  const entry=git(repo,['ls-files','--stage','--','integrations/webmail/bulwark']);
  const match=/^160000 ([0-9a-f]{40,64}) 0\t/.exec(entry);
  if(!match)throw new Error('Missing or conflicted pinned webmail gitlink');
  if(existsSync(resolve(source,'package.json')) && git(source,['status','--porcelain','--untracked-files=normal'])) {
    throw new Error('Pinned webmail source is dirty; preserve those edits before building');
  }
  if(!existsSync(resolve(source,'package.json')) || git(source,['rev-parse','HEAD'])!==match[1]) {
    execFileSync('git',['submodule','update','--init','--checkout','--','integrations/webmail/bulwark'],{cwd:repo,stdio:'inherit'});
  }
  if(git(source,['rev-parse','HEAD'])!==match[1])throw new Error('Webmail source does not match the recorded pin');
  return match[1];
}
