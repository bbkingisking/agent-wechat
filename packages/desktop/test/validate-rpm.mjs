// Inspect and exercise a personal Fedora ARM64 package without touching user data.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const path = process.argv[2];
if (!path) throw new Error('Pass the built RPM path');
const rpm = resolve(path);
const metadata = execFileSync('rpm', ['-qp', '--qf', '%{NAME}\n%{ARCH}\n%{RELEASE}\n', rpm], { encoding: 'utf8' }).trim().split('\n');
assert.equal(metadata[0], 'agent-wechat');
assert.equal(metadata[1], 'aarch64');
assert.match(metadata[2], /^\d+(?:\.\d+)?\.fc45$/);
const paths = execFileSync('rpm', ['-qpl', rpm], { encoding: 'utf8' }).trim().split('\n');
const helpers = ['wx', 'ffmpeg', 'ffprobe'];
const executables = ['/usr/bin/agent-wechat', ...helpers.map(name => `/usr/lib/AgentWechat/bin/${name}`)];
for (const entry of executables) assert.ok(paths.includes(entry), `Missing executable: ${entry}`);
for (const name of helpers) assert.ok(!paths.includes(`/usr/bin/${name}`), 'Helpers must not replace system commands');
assert.ok(paths.some(name => name.endsWith('.desktop')), 'Missing application launcher');
assert.ok(paths.some(name => name.startsWith('/usr/share/icons/')), 'Missing application icon');
assert.ok(paths.some(name => name.startsWith('/usr/lib/AgentWechat/licenses/')), 'Missing bundled media notices');
const sound = '/usr/lib/AgentWechat/sounds/AgentWechatSoftPair.wav';
assert.ok(paths.includes(sound), 'Missing notification sound');

const directory = mkdtempSync(join(tmpdir(), 'agent-wechat-rpm-'));
try {
  const archive = execFileSync('rpm2cpio', [rpm], { maxBuffer: 512 * 1024 * 1024 });
  execFileSync('cpio', ['--extract', '--make-directories', '--no-absolute-filenames', '--quiet'], { cwd: directory, input: archive });
  for (const entry of executables) {
    const executable = join(directory, entry);
    const bytes = readFileSync(executable);
    assert.equal(bytes.subarray(0, 6).toString('hex'), '7f454c460201', `Expected 64-bit little-endian ELF: ${entry}`);
    assert.equal(bytes.readUInt16LE(18), 183, `Expected ARM64 executable: ${entry}`);
    // Static helpers need no dynamic dependencies; ldd exits nonzero for them.
    let libraries;
    try {
      libraries = execFileSync('ldd', [executable], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
    } catch (error) {
      const output = `${error.stdout ?? ''}${error.stderr ?? ''}`;
      assert.match(output, /not a dynamic executable|statically linked/, `Cannot inspect dependencies: ${entry}`);
      libraries = output;
    }
    assert.ok(!libraries.includes('not found'), `Unresolved dependencies: ${entry}\n${libraries}`);
  }
  assert.deepEqual(readFileSync(join(directory, sound)), readFileSync(new URL('../src-tauri/sounds/AgentWechatSoftPair.wav', import.meta.url)));
  for (const name of helpers) {
    execFileSync(join(directory, 'usr/lib/AgentWechat/bin', name), [name === 'wx' ? '--version' : '-version'], { stdio: 'pipe', timeout: 30000 });
  }
  console.log('Fedora RPM contains runnable ARM64 executables, private helpers, launcher, icon, licenses and sound');
} finally {
  rmSync(directory, { recursive: true, force: true });
}
