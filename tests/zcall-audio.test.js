const test = require('node:test');
const assert = require('node:assert/strict');
const { spawn, spawnSync } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');

test('Native zcall-audio engine full IPC protocol and state transitions', { timeout: 20000 }, async (t) => {
  const binaryPath = path.join(__dirname, '..', 'target', 'release', 'zcall-audio');
  if (!fs.existsSync(binaryPath)) {
    t.skip('zcall-audio release binary not built yet');
    return;
  }

  // Without libopus the engine cannot encode: nothing to test here.
  const check = spawnSync(binaryPath, ['--check'], { encoding: 'utf8' });
  if (/\bopus\b/.test(check.stdout)) {
    t.skip('libopus not installed');
    return;
  }

  const proc = spawn(binaryPath, ['--tone'], {
    stdio: ['pipe', 'pipe', 'pipe'],
  });

  // A failed assertion must not leave the engine running (the test would hang).
  t.after(() => { if (proc.exitCode === null) proc.kill('SIGKILL'); });

  let stderrLogs = '';
  proc.stderr.on('data', (d) => {
    stderrLogs += d.toString('utf8');
  });

  const frames = [];
  let buffer = Buffer.alloc(0);

  const readNextFrame = () => {
    return new Promise((resolve) => {
      const check = () => {
        while (buffer.length >= 4) {
          if (buffer[0] === 0x4c) {
            const level = buffer[1];
            const len = buffer.readUInt16BE(2);
            if (buffer.length < 4 + len) break;
            const payload = buffer.subarray(4, 4 + len);
            buffer = buffer.subarray(4 + len);
            return resolve({ level, len, payload });
          }
          buffer = buffer.subarray(1);
        }
        proc.stdout.once('data', (chunk) => {
          buffer = Buffer.concat([buffer, chunk]);
          check();
        });
      };
      check();
    });
  };

  // 1. Verify initial audio frames with test tone (should have speech/tone loudness level < 127)
  const initialFrame = await readNextFrame();
  assert.equal(typeof initialFrame.level, 'number', 'Audio level must be a number');
  assert.ok(initialFrame.level < 100, `Tone frame level (${initialFrame.level} dBov) must reflect audible signal (<100)`);
  assert.ok(initialFrame.len > 0, 'Opus payload must not be empty');
  assert.match(stderrLogs, /\[audio-io\] audio config: /, 'Engine must log its audio config');

  // 2. Volume 'V' (percent: 75). With --tone there is no PulseAudio source to
  // turn, and the engine says so instead of faking a level for the slider.
  proc.stdin.write(Buffer.from([0x56, 75]));
  await new Promise((r) => setTimeout(r, 50));
  assert.match(stderrLogs, /\[audio-io\] mic volume: /, 'Stderr must answer the volume request');

  // 3. Test Mute command 'M' (mute: 1)
  proc.stdin.write(Buffer.from([0x4d, 1]));
  // Wait a couple frames to let mute take effect
  await readNextFrame();
  const mutedFrame = await readNextFrame();
  assert.equal(mutedFrame.level, 127, 'Muted audio frame must have RFC 6464 level 127 (silence)');

  // 4. Test Unmute command 'M' (mute: 0)
  proc.stdin.write(Buffer.from([0x4d, 0]));
  await readNextFrame();
  const unmutedFrame = await readNextFrame();
  assert.ok(unmutedFrame.level < 100, `Unmuted audio frame level (${unmutedFrame.level}) must return to audible`);

  // 5. Test Inbound frame playback 'F' using the captured valid Opus packet
  // Header: 'F' (0x46), u32 idx (0x01), u32 ssrc (0x1234), u16 len, payload
  const inboundHdr = Buffer.alloc(11);
  inboundHdr[0] = 0x46; // 'F'
  inboundHdr.writeUInt32BE(1, 1); // idx 1
  inboundHdr.writeUInt32BE(0x1234, 5); // ssrc
  inboundHdr.writeUInt16BE(unmutedFrame.payload.length, 9);
  proc.stdin.write(Buffer.concat([inboundHdr, unmutedFrame.payload]));

  // 6. Test Speaker off command 'S' (off: 1)
  proc.stdin.write(Buffer.from([0x53, 1]));
  await new Promise((r) => setTimeout(r, 50));
  assert.ok(stderrLogs.includes('[audio-io] speaker off'), 'Stderr must acknowledge speaker off');

  // 7. Test Device switch command 'D' (mic device)
  const devName = Buffer.from('default_test_mic', 'utf8');
  const devHdr = Buffer.from([0x44, 0x69, devName.length >> 8, devName.length & 0xff]);
  proc.stdin.write(Buffer.concat([devHdr, devName]));
  await new Promise((r) => setTimeout(r, 50));
  assert.ok(stderrLogs.includes('device change: microphone default_test_mic'), 'Stderr must acknowledge device switch');

  // 8. Clean shutdown on stdin EOF
  proc.stdin.end();

  const exitCode = await new Promise((resolve) => {
    proc.on('exit', (code) => resolve(code));
  });

  assert.equal(exitCode, 0, 'zcall-audio process must exit cleanly with code 0 on EOF');
  assert.match(stderrLogs, /\[audio-io\] stats \{/, 'Engine must log its playout stats at exit');
});

test('zcall-audio --check names what calls still need', (t) => {
  const binaryPath = path.join(__dirname, '..', 'target', 'release', 'zcall-audio');
  if (!fs.existsSync(binaryPath)) {
    t.skip('zcall-audio release binary not built yet');
    return;
  }
  const res = spawnSync(binaryPath, ['--check'], { encoding: 'utf8' });
  assert.equal(res.status, 0);
  const needs = res.stdout.trim().split(/\s+/).filter(Boolean);
  for (const n of needs) assert.ok(['opus', 'audio'].includes(n), `unexpected need ${n}`);
});
