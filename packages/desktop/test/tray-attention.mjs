// Run the installed app on a private bus/display with synthetic HTTP/SSE only.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { createInterface } from 'node:readline';

assert.equal(process.env.TRAY_ICON_TEST_SESSION, '1');
assert.ok(process.env.DESKTOP_BINARY);
assert.ok(process.env.XDG_CONFIG_HOME);
let watcher;
let application;
let log = '';
let ready = false;
let current;
let serial = 1;
let unread = 0;
let muted = false;
let partial = false;
let authenticated = true;
let scanned = 0;
let openedStreams = 0;
const signals = [];
const streams = new Set();
const events = [];
const mutations = [];
const key = 'synthetic-access-key-for-private-tray-test';
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(predicate, label, timeout = 20000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (predicate()) return;
    if (application?.exitCode !== null && application?.exitCode !== undefined) throw new Error(`App exited: ${log}`);
    await pause(50);
  }
  throw new Error(`Timed out: ${label}; signals=${JSON.stringify(signals)}; log=${log}`);
}
const hub = createServer((req, res) => {
  assert.equal(req.headers.authorization, `Bearer ${key}`);
  const url = new URL(req.url, 'http://localhost');
  if (req.method !== 'GET') mutations.push(`${req.method} ${url.pathname}`);
  if (url.pathname.endsWith('/events/stream')) {
    res.writeHead(200, {'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache'});
    res.write(': fixture\n\n');
    streams.add(res);
    openedStreams++;
    res.on('close', () => streams.delete(res));
    const after = BigInt(url.searchParams.get('after') ?? '0');
    for (const event of events) {
      if (BigInt(event.cursor) > after) res.write(`data: ${JSON.stringify(event)}\n\n`);
    }
    return;
  }
  const json = value => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
  if (url.pathname === '/v1/status') return json({serverId:'tray-fixture', version:{backend:'native'}});
  if (url.pathname === '/v1/accounts') return json({items:[{id:'fixture',alias:'Tray fixture',authenticated}]});
  if (url.pathname === '/v1/accounts/fixture') return json({authenticated,contactSyncReady:true});
  if (url.pathname.endsWith('/threads')) {
    scanned = serial;
    return json({items:[{
      id:'peer',accountId:'fixture',peerId:'peer',kind:'direct',displayName:'Fixture',
      unreadCount:unread,muted,readStateVersion:String(serial),history:{}
    }],nextCursor:null,sourceGeneration:String(serial),asOfEventCursor:String(serial),coverage:{partial,databases:[]}});
  }
  if (url.pathname.endsWith('/capabilities')) return json({});
  return json({items:[],nextCursor:null,asOfEventCursor:String(serial)});
});
function update(values, kind = 'thread.updated') {
  if ('unread' in values) unread = values.unread;
  if ('muted' in values) muted = values.muted;
  if ('partial' in values) partial = values.partial;
  if ('authenticated' in values) authenticated = values.authenticated;
  serial++;
  const event = {cursor:String(serial),kind,data:{threadId:'peer'},createdAt:new Date().toISOString()};
  events.push(event);
  for (const stream of streams) stream.write(`data: ${JSON.stringify(event)}\n\n`);
}
async function expectStatus(values, expected, kind) {
  const before = signals.length;
  update(values, kind);
  await until(() => scanned === serial, 'thread snapshot refreshed');
  await until(() => signals.length > before && current === expected, `tray ${expected}`);
}
try {
  await new Promise(resolve => hub.listen(0, '127.0.0.1', resolve));
  const endpoint = `http://127.0.0.1:${hub.address().port}`;
  watcher = spawn('python3', [new URL('./tray-watcher.py', import.meta.url).pathname], {stdio:['ignore','pipe','pipe']});
  watcher.stderr.on('data', bytes => { log = (log + bytes).slice(-32768); });
  createInterface({input:watcher.stdout}).on('line', line => {
    const event = JSON.parse(line);
    if (event.event === 'ready') ready = true;
    if (event.event === 'status') {
      assert.equal(event.value, event.property);
      current = event.property;
      signals.push(current);
    }
  });
  await until(() => ready, 'private watcher ready');
  const installation = randomUUID();
  const settings = join(process.env.XDG_CONFIG_HOME, 'io.github.thisnick.agent-wechat', 'settings');
  await mkdir(settings, {recursive:true,mode:0o700});
  await writeFile(join(settings, 'desktop.json'), JSON.stringify({
    schemaVersion:1,installationId:installation,active:'fixture',
    connections:[{id:'fixture',name:'Tray fixture',endpoint,local:false,serverId:'tray-fixture',certificate:null}],
    localPort:6174,networkPort:6175,networkEnabled:false,
    notificationsEnabled:false,notificationSoundEnabled:false,notificationDisabledAccounts:[],
    automaticUpdates:false,updatePrereleases:false,language:'en',keepLocalHubRunning:false
  }), {mode:0o600});
  // Same documented Secret Service attributes used by the keyring crate.
  execFileSync('secret-tool', ['store','--label=Private tray test',
    'service','io.github.thisnick.agent-wechat','username',`${installation}:fixture`,'target','default'], {input:key,timeout:10000});
  application = spawn(process.env.DESKTOP_BINARY, [], {stdio:['ignore','pipe','pipe']});
  application.stderr.on('data', bytes => { log = (log + bytes).slice(-32768); });
  application.stdout.on('data', bytes => { log = (log + bytes).slice(-32768); });
  await until(() => current === 'Active' && openedStreams > 0, 'initial active tray and native subscription');
  // Popup notifications are disabled throughout: tray attention is independent.
  await expectStatus({unread:2}, 'NeedsAttention', 'message.committed');
  await expectStatus({unread:0}, 'Active', 'thread.read-state');
  await expectStatus({unread:3}, 'NeedsAttention');
  await expectStatus({muted:true}, 'Active');
  await expectStatus({muted:false}, 'NeedsAttention');
  const previousStreams = openedStreams;
  update({unread:0,partial:true}, 'thread.read-state');
  await until(() => scanned === serial && openedStreams > previousStreams, 'partial snapshot read and stream resumed');
  assert.equal(current, 'NeedsAttention', 'incomplete data must not clear attention');
  await expectStatus({partial:false}, 'Active', 'thread.read-state');
  await expectStatus({unread:2}, 'NeedsAttention');
  update({authenticated:false}, 'account.updated');
  await until(() => current === 'Active', 'signed-out account cleared');
  update({authenticated:true}, 'account.updated');
  await until(() => current === 'NeedsAttention', 'signed-in account restored');
  await expectStatus({unread:0}, 'Active', 'thread.read-state');
  assert.deepEqual(mutations, [], 'attention is read-only');
  console.log('Desktop tray passed actual Status/NewStatus checks for unread, read, mute, partial data and account lifecycle with popups off');
} finally {
  application?.kill('SIGTERM');
  watcher?.kill('SIGTERM');
  for (const stream of streams) stream.end();
  hub.closeAllConnections();
  await new Promise(resolve => hub.close(resolve));
}
