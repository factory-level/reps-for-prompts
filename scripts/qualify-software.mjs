// Repeatable software evidence. Never activates movements or opens a camera.
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync, rmSync, openSync, closeSync, lstatSync, readlinkSync } from 'node:fs';
import { tmpdir, platform, release, cpus, homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const hub = resolve(process.env.HUB_DIR ?? join(root, '../usb-mcp-hub'));
const args = process.argv.slice(2);
if (args.includes('--help')) {
  console.log('node scripts/qualify-software.mjs [--output DIR] [--with-video] [--with-bundle] [--python-env DIR]\nRequires provisioned locked dependencies. Writes reports/logs; uses disposable test data.');
  process.exit(0);
}
let output, pythonEnv;
let withVideo = false, withBundle = false;
for (let i=0;i<args.length;i++) {
  if (args[i] === '--with-video') withVideo = true;
  else if (args[i] === '--with-bundle') withBundle = true;
  else if (['--output','--python-env'].includes(args[i]) && args[i+1] && !args[i+1].startsWith('--')) {
    const key = args[i++]; if (key === '--output') output = resolve(args[i]); else pythonEnv = resolve(args[i]);
  } else throw new Error(`Unknown or incomplete option: ${args[i]}`);
}
output ??= mkdtempSync(join(tmpdir(), 'reps-qualification-report-'));
mkdirSync(output,{recursive:true});
const temporary = mkdtempSync(join(tmpdir(), 'reps-qualification-state-'));
const sha = file => existsSync(file) ? createHash('sha256').update(readFileSync(file)).digest('hex') : null;
function git(cwd, args) {
  const r = spawnSync('git', args, {cwd,encoding:'utf8'});
  if (r.status !== 0) throw new Error(`Cannot inspect repository ${cwd}: ${r.stderr}`);
  return r.stdout.trim();
}
function identity(cwd) {
  const status = git(cwd,['status','--porcelain']);
  const paths = spawnSync('git',['ls-files','--cached','--others','--exclude-standard','-z'],{cwd,encoding:'utf8'});
  if (paths.status !== 0) throw new Error(`Cannot enumerate sources in ${cwd}`);
  const files = [...new Set(paths.stdout.split('\0').filter(Boolean))].sort();
  const tree = createHash('sha256');
  for (const file of files) {
    const path = join(cwd,file);
    const stat = lstatSync(path,{throwIfNoEntry:false});
    const content = stat?.isSymbolicLink() ? `symlink:${readlinkSync(path)}` : stat?.isDirectory() ? 'directory' : sha(path);
    tree.update(file+'\0'+content+'\0');
  }
  return {commit:git(cwd,['rev-parse','HEAD']),dirty:!!status,status,
    workingTreeSha256:tree.digest('hex'),
    diffSha256:createHash('sha256').update(git(cwd,['diff','HEAD','--binary'])).digest('hex')};
}
const report = {schemaVersion:1, startedAt:new Date().toISOString(), evidence:'software-only',productionQualified:false,
  host:{platform:platform(),release:release(),cpu:cpus()[0]?.model,node:process.version},
  sources:{reps:identity(root),hub:identity(hub)},
  artifacts:{manifestSha256:sha(join(root,'app/src-tauri/resources/hub-manifest.json')),
    hubBundleSha256:sha(join(root,'app/src-tauri/resources/hub-bundle/hubd.mjs')),
    modelSha256:sha(join(root,'app/src-tauri/resources/models/pose_landmarker_full.task'))},checks:[],
  pendingReleaseGates:[
    'Actual gym calibration and held-out evidence for squat, push-up and curl',
    'Target-machine rep-to-display p95 below 300 ms',
    'Physical camera interruption and emergency escape',
    'Intended orchestrator effects and destination-specific deduplication',
    'Installed desktop UI restart while hub is offline',
    'Physical full-volume behavior (automated checks use real SQLITE_FULL page limits)',
    'Activation during a physical workout and rollback',
    'Installed offline workout and eight-hour soak',
    'Consented account-specific cloud authoring',
    'Hosted CI execution and backups/capacity monitoring',
  ]};
const env = {...process.env,UV_CACHE_DIR:process.env.UV_CACHE_DIR ?? join(homedir(),'.cache/uv'),UV_OFFLINE:'1',UV_FROZEN:'1',
  HUB_DATA_DIR:join(temporary,'hub'),REPS_HOME:join(temporary,'reps'),PYTHONDONTWRITEBYTECODE:'1',
  REPS_POSE_MODEL:join(root,'app/src-tauri/resources/models/pose_landmarker_full.task')};
// Do not inherit opt-in cameras, plugins, or authoring settings into test hosts.
for (const key of Object.keys(env)) if (/^(HUB_DEMO_|HUB_PLUGIN|ANTHROPIC_API_KEY)/.test(key)) delete env[key];
let activeChild;
function killOwnedChild(signal = 'SIGTERM') {
  if (!activeChild || activeChild.exitCode !== null || activeChild.signalCode !== null) return;
  try {process.kill(-activeChild.pid,signal);} catch {activeChild.kill(signal);}
}
for (const signal of ['SIGINT','SIGTERM']) process.on(signal,()=>{
  report.interrupted = signal;killOwnedChild();save();
  // The child's close event completes the current check and the finally block
  // removes temporary state after owned processes have exited.
});
function save() {
  report.finishedAt = new Date().toISOString();
  report.passed = !report.interrupted && report.checks.length > 0 && report.checks.every(c=>c.status === 'passed');
  writeFileSync(join(output,'report.json'),JSON.stringify(report,null,2)+'\n');
  writeFileSync(join(output,'summary.md'),`# Software qualification\n\n${report.passed ? 'PASS' : 'INCOMPLETE / FAIL'} — software evidence only. No movement is production-qualified.\n\n`+
    report.checks.map(c=>`- ${c.status.toUpperCase()}: ${c.name} (${c.durationMs} ms); ${c.log}`).join('\n')+
    '\n\n## Public-video challenge failures\n\n'+(report.challengeFailures?.join(', ') || 'See requested checks; no challenge failure reported.')+
    '\n\n## Pending release gates\n\n'+report.pendingReleaseGates.map(g=>`- ${g}`).join('\n')+'\n');
}
async function run(name,cwd,command,argv,timeoutMs=300000) {
  if (report.interrupted) return;
  const log = `${name}.log`, fd = openSync(join(output,log),'w');
  console.log(`Checking ${name}…`);
  const started = Date.now();
  const result = await new Promise(resolveResult=> {
    const child = spawn(command,argv,{cwd,env,stdio:['ignore',fd,fd],detached:process.platform !== 'win32'});
    activeChild = child;
    let error, timedOut=false, forceTimer;
    child.once('exit',()=>{clearTimeout(forceTimer);});
    const heartbeat = setInterval(()=>console.log(`Still checking ${name} (${Math.round((Date.now()-started)/1000)}s)…`),30000);
    const timer = setTimeout(()=>{
      timedOut=true;
      killOwnedChild();
      forceTimer = setTimeout(()=>killOwnedChild('SIGKILL'),10000);
    },timeoutMs);
    child.once('error',e=>{error=e.message;});
    child.once('close',(code,signal)=>{clearTimeout(timer);clearTimeout(forceTimer);clearInterval(heartbeat);resolveResult({code,signal,error,timedOut});});
  });
  closeSync(fd);
  activeChild = undefined;
  const check = {name,status:result.code === 0 && !result.timedOut ? 'passed' : result.error ? 'blocked' : 'failed',
    command:[command,...argv],...result,durationMs:Date.now()-started,log};
  report.checks.push(check);save();console.log(`${name}: ${check.status}`);
}
try {
  await run('hub-types',hub,'pnpm',['typecheck']);
  await run('hub-tests',hub,'pnpm',['test']);
  await run('hub-python',join(hub,'vision'),'uv',['run','--frozen','python','-m','pytest']);
  await run('detector',join(root,'vision'),'uv',['run','--frozen','--extra','cv','python','-m','pytest']);
  await run('desktop-tests',join(root,'app'),'pnpm',['test']);
  await run('desktop-types',join(root,'app'),'pnpm',['exec','tsc','--noEmit']);
  await run('desktop-rust',join(root,'app/src-tauri'),'cargo',['test','--workspace','--locked','--offline'],600000);
  if (withVideo) {
    await run('public-video',join(root,'vision'),'uv',['run','--frozen','--extra','cv','python','scripts/benchmark_movements.py','--output',join(output,'public-video.json')],300000);
    if (existsSync(join(output,'public-video.json'))) report.challengeFailures = JSON.parse(readFileSync(join(output,'public-video.json'),'utf8')).challengeFailures;
  }
  if (withBundle) await run('bundle',root,'node',['scripts/e2e-latency.mjs','--bundle','--movement-contract','--parent-exit','--offline',...(pythonEnv?['--python-env',pythonEnv]:[])],240000);
  report.optionalChecks={publicVideo:withVideo?'requested':'not requested',bundle:withBundle?'requested':'not requested'};
} finally {save();rmSync(temporary,{recursive:true,force:true});}
console.log(`Report: ${join(output,'summary.md')}`);
process.exitCode = report.passed ? 0 : 1;
