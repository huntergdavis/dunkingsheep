import { verifySource } from './source.mjs';
// Build a pinned upstream client in a disposable workspace, never in the submodule.
import { cpSync, mkdirSync, mkdtempSync, rmSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
const root = dirname(fileURLToPath(import.meta.url));
verifySource(root);
const scratch = process.env.DS_MAIL_BUILD_DIR ? resolve(process.env.DS_MAIL_BUILD_DIR) : mkdtempSync(resolve(root,'../../../jmap-client-build-'));
const freshlyCreated = !process.env.DS_MAIL_BUILD_DIR;
if (scratch === resolve(root,'bulwark')) throw new Error('Refusing to modify pinned source');
if (existsSync(scratch) && !freshlyCreated) throw new Error(`Use a new DS_MAIL_BUILD_DIR: ${scratch}`);
mkdirSync(scratch, {recursive:true});
cpSync(resolve(root,'bulwark'),scratch,{recursive:true,filter: p => !p.endsWith('/.git')});
execFileSync('git',['apply',resolve(root,'security.patch')],{cwd:scratch,stdio:'inherit'});
function replace(file, before, after) {
  const path=resolve(scratch,file), source=readFileSync(path,'utf8');
  if (!source.includes(before)) throw new Error(`Upstream patch no longer applies: ${file}`);
  writeFileSync(path,source.replace(before,after));
}
replace('lib/jmap/client.ts','function withJmapCredentials(init: RequestInit | undefined): RequestInit {',`function withJmapCredentials(init: RequestInit | undefined): RequestInit {
  const headers = new Headers(init?.headers);
  headers.delete('Authorization');
  return { ...init, headers, credentials: 'same-origin' };
}
function unusedUpstreamCredentials(init: RequestInit | undefined): RequestInit {`);
// t5484: the adapter delivers only to the desk, so a new message starts addressed to it.
replace('components/email/email-composer.tsx',"if (mode !== 'reply' && mode !== 'replyAll') return [];","if (mode !== 'reply' && mode !== 'replyAll') return [{ name: 'Front Desk', email: 'desk@dunkingsheep.invalid' }];");
replace('components/mail/mail-app.tsx','import { useEffect,',`import { connectDunkingSheep } from '@/lib/dunkingsheep';\nimport { useEffect,`);
replace('components/mail/mail-app.tsx','checkAuth().finally(() => {','connectDunkingSheep().finally(() => {');
writeFileSync(resolve(scratch,'lib/dunkingsheep.ts'), `
import { JMAPClient } from '@/lib/jmap/client';
import { useAuthStore } from '@/stores/auth-store';
import { useAccountStore } from '@/stores/account-store';
export async function connectDunkingSheep() {
  const client = JMAPClient.withBearer(window.location.origin, 'cookie-session', '');
  await client.connect();
  const identities = await client.getIdentities();
  const accounts=useAccountStore.getState();
  const accountId=accounts.addAccount({serverUrl:window.location.origin,username:'owner@dunkingsheep.invalid',label:'Dunking Sheep',displayName:'You',email:'owner@dunkingsheep.invalid',authMode:'token',rememberMe:false,lastLoginAt:Date.now(),isConnected:true,hasError:false,isDefault:true});
  accounts.setActiveAccount(accountId);
  useAuthStore.setState({ activeAccountId:accountId,getClientForAccount:(id:string)=>id===accountId?client:undefined,getAllConnectedClients:()=>new Map([[accountId,client]]),client, isAuthenticated: true, isLoading: false,
    serverUrl: window.location.origin, username: 'owner@dunkingsheep.invalid',
    identities, primaryIdentity: identities[0] ?? null, connectionLost: false, error: null });
}
`);
const env={...process.env,CI:'true',NEXT_TELEMETRY_DISABLED:'1',LITE_TARGET:'static',NEXT_PUBLIC_BASE_PATH:'/webmail',LITE_LOCALES:'en',LITE_APP_NAME:'Dunking Sheep Inbox',LITE_ALLOW_CUSTOM_ENDPOINT:'false',LITE_NO_THIRD_PARTY_TRAFFIC:'true'};
execFileSync('npm',['ci','--ignore-scripts','--no-audit','--no-fund'],{cwd:scratch,env,stdio:'inherit'});
execFileSync('npm',['run','build:lite'],{cwd:scratch,env,stdio:'inherit'});
const destination=resolve(root,'../../ui/public/webmail');
rmSync(destination,{recursive:true,force:true});
cpSync(resolve(scratch,'out'),destination,{recursive:true});
if(freshlyCreated)rmSync(scratch,{recursive:true,force:true});
