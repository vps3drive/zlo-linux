'use strict';

// ZCALL_MEDIA=rust: backend choice per call, and one scripted 1-1 call
// against a fake relay through the zcall-media process (skipped until
// target/release/zcall-media and zcall-audio are built).

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const dgram = require('dgram');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { ZCALL_DIR, ROOT } = require('../scripts/utils/paths');

const ENG = path.join(ZCALL_DIR, 'native-engine');
const z = require(path.join(ENG, 'zrtp-packet'));
const srtp = require(path.join(ENG, 'srtp-aes'));
const rtp = require(path.join(ENG, 'rtp'));
const { ZrtcMediaBackend } = require(path.join(ENG, 'backends', 'zrtc-media'));
const { packetizeFrame } = require(path.join(ENG, 'video'));
const { RustMediaBackend, MediaSwitch, createRustMedia } = require(path.join(ENG, 'backends', 'zrtc-media-rust'));

const MEDIA_BIN = path.join(ROOT, 'target', 'release', 'zcall-media');
const AUDIO_BIN = path.join(ROOT, 'target', 'release', 'zcall-audio');

describe('ZCALL_MEDIA=rust backend choice', () => {
  it('runs group calls on zrtc-media.js and 1-1 calls on zcall-media', () => {
    const logs = [];
    const sw = new MediaSwitch({ log: (...a) => logs.push(a.join(' ')), sendUdp: false, bin: '/nonexistent' });
    const calls = [];
    sw.js.onNegotiated = (p) => calls.push(['js', p.group]);
    sw.rust.onNegotiated = (p) => calls.push(['rust', p.group]);
    sw.onNegotiated({ group: true });
    sw.onNegotiated({ group: false });
    assert.deepStrictEqual(calls, [['js', true], ['rust', false]]);
    assert.strictEqual(sw.cur, sw.rust);
  });

  it('forwards only the current backend\'s events', () => {
    const sw = new MediaSwitch({ log: () => {}, sendUdp: false, bin: '/nonexistent' });
    sw.cur = sw.rust;
    const got = [];
    sw.on('timeout', () => got.push('timeout'));
    sw.js.emit('timeout');
    sw.rust.emit('timeout');
    assert.deepStrictEqual(got, ['timeout']);
  });

  it('keeps device, mute and speaker choices for both backends', () => {
    const sw = new MediaSwitch({ log: () => {}, sendUdp: false, bin: '/nonexistent' });
    sw.setDevice('mic', 'alsa_input.usb');
    assert.strictEqual(sw.js.devices.mic, 'alsa_input.usb');
    assert.strictEqual(sw.rust.devices.mic, 'alsa_input.usb');
  });

  it('falls back to zrtc-media.js when the binary is missing', (t) => {
    if (fs.existsSync(MEDIA_BIN) || fs.existsSync(path.join(ENG, 'zcall-media'))) {
      t.skip('zcall-media is built here');
      return;
    }
    const logs = [];
    const m = createRustMedia({ log: (l) => logs.push(l), sendUdp: false });
    assert.ok(m instanceof ZrtcMediaBackend);
    assert.match(logs.join('\n'), /binary is missing/);
  });
});

// Opus frames of zcall-audio's test tone, for the fake phone's downlink.
function toneFrames(n) {
  return new Promise((resolve, reject) => {
    const p = spawn(AUDIO_BIN, ['--tone', '--out', '/dev/null'], { stdio: ['pipe', 'pipe', 'ignore'] });
    let buf = Buffer.alloc(0);
    const out = [];
    p.on('error', reject);
    p.stdout.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 4 && buf.length >= 4 + buf.readUInt16BE(2)) {
        const len = buf.readUInt16BE(2);
        out.push(Buffer.from(buf.subarray(4, 4 + len)));
        buf = buf.subarray(4 + len);
      }
      if (out.length >= n) { p.kill(); resolve(out.slice(0, n)); }
    });
  });
}

describe('zcall-media process: a 1-1 call against a fake relay', () => {
  it('connects, plays the downlink, sends SRTP audio and video, and hangs up', { timeout: 30000 }, async (t) => {
    if (!fs.existsSync(MEDIA_BIN) || !fs.existsSync(AUDIO_BIN)) {
      t.skip('zcall-media / zcall-audio release binaries not built yet');
      return;
    }
    const outDir = fs.mkdtempSync(path.join(os.tmpdir(), 'zcall-media-test-'));
    t.after(() => fs.rmSync(outDir, { recursive: true, force: true }));
    const saved = { args: process.env.ZCALL_AUDIO_ARGS, diag: process.env.ZCALL_AUDIO_DIAG };
    process.env.ZCALL_AUDIO_ARGS = `--tone --out ${path.join(outDir, 'played.raw')}`;
    process.env.ZCALL_AUDIO_DIAG = '0';
    t.after(() => {
      for (const [k, v] of [['ZCALL_AUDIO_ARGS', saved.args], ['ZCALL_AUDIO_DIAG', saved.diag]]) {
        if (v === undefined) delete process.env[k]; else process.env[k] = v;
      }
    });

    const frames = await toneFrames(50);
    const sessId = 'S'.repeat(20) + '0123456789abcdefghijklmnopqrstuvwxyz';
    const keys = srtp.sessionKeys(srtp.masterFromSessId(sessId));
    const LOCAL = 111222333, PEER = 444555666, CALL = 777888, TOKEN = 0x01020304;
    const seen = { up: 0, upBad: 0, endCall: 0, twcc: 0, p2pBind: 0, nack: 0, upFrames: [] };
    let upParts = [];
    const server = dgram.createSocket('udp4');
    const peer = dgram.createSocket('udp4');
    t.after(() => { server.close(); peer.close(); });
    await new Promise((r) => server.bind(0, '127.0.0.1', r));
    await new Promise((r) => peer.bind(0, '127.0.0.1', r));
    let client = null;
    server.on('message', (m, rinfo) => {
      client = rinfo;
      if (m[0] === 1) {
        const p = z.unwrapPacket(m);
        if (p.cmd === 11) {
          const h = z.buildControlHeader({ msgType: 2, cmd: 11, subCmd: 2, uid: LOCAL });
          const addr = Buffer.from(`127.0.0.1|${rinfo.port}`, 'latin1');
          const body = Buffer.alloc(14);
          body.writeUInt32LE(CALL, 4); body.writeUInt32LE(TOKEN, 8); body.writeUInt16LE(addr.length, 12);
          server.send(Buffer.concat([h, body, addr]), rinfo.port, rinfo.address);
        } else if (p.cmd === 3) seen.endCall++;
      } else if (m[0] === 3) {
        if (m.readUInt32LE(1) !== TOKEN) seen.upBad++;
        else if (srtp.unprotectRtp(m.subarray(5), keys, 0)) seen.up++;
        else seen.upBad++;
      } else if (m[0] === 5) seen.twcc++;
      else if (m[0] === 13) {
        // Our camera: decrypt and put the frame back together.
        const un = srtp.unprotectRtp(m.subarray(5), keys, 0);
        if (!un) { seen.upBad++; return; }
        const p = un.opus;
        upParts.push(p.length >= 2 && p[0] === 0x1c && (p[1] & 0x3f) === 0 ? p.subarray(2) : p);
        if (m[6] & 0x80) { seen.upFrames.push(Buffer.concat(upParts)); upParts = []; }
      } else if (m[0] === 15) {
        for (let o = 5; o + 4 <= m.length; o += (m.readUInt16BE(o + 2) + 1) * 4) if (m[o + 1] === 205 && (m[o] & 0x1f) === 1) seen.nack++;
      }
    });
    peer.on('message', (m) => { if (m[7] === 1) seen.p2pBind++; });

    const logs = [];
    const media = new RustMediaBackend({ log: (...a) => logs.push(a.join(' ')), sendUdp: true, bin: MEDIA_BIN });
    const events = [];
    for (const ev of ['timeout', 'failed', 'peerEnd', 'audioFault', 'keyframe']) media.on(ev, (...a) => events.push([ev, ...a]));
    const video = [];
    media.on('video', (v) => video.push(v));
    media.onNegotiated({ servers: [{ rtpaddr: `127.0.0.1:${server.address().port}` }], config: {}, srtpMode: 1, sessId,
      callId: CALL, role: 'caller', localUid: LOCAL, peerUid: PEER, p2pCandidates: [{ ip: '127.0.0.1', port: peer.address().port }] });
    const best = await media.prepare('caller');
    assert.strictEqual(best.token, TOKEN);
    assert.strictEqual(best.publicAddr, `127.0.0.1|${media.port}`);
    media.start('caller', null);
    // The phone's video: 10 frames of 3 fragments; one fragment lost (NACK).
    const phoneFrames = [];
    let vseq = 100;
    for (let f = 0; f < 10; f++) {
      const frame = Buffer.concat([Buffer.from([0, 0, 0, 1, f === 0 ? 0x67 : 0x41]), Buffer.alloc(2500, f + 1)]);
      phoneFrames.push(frame);
      packetizeFrame(frame).forEach((ch, ci, all) => {
        const s = vseq++;
        if (s === 112) return;
        const hdr = rtp.buildAudioRtpHeader({ seq: s, ts: 9000 * f, ssrc: PEER, twSeq: 1000 + s, marker: ci === all.length - 1, pt: f === 0 ? 98 : 97 });
        server.send(Buffer.concat([Buffer.from([0x0e]), srtp.protectRtp(hdr, ch, keys, s, PEER, 0)]), client.port, client.address);
      });
    }
    // Our camera: a delta before any key frame is dropped (and a key frame
    // asked for), then a 20 KB key frame (paced) and two deltas.
    const camera = [false, true, false, false].map((key, n) => ({ key, data: Buffer.concat([Buffer.from([0, 0, 0, 1, key ? 0x65 : 0x41]), Buffer.alloc(key ? 20000 : 3000, n + 7)]) }));
    for (const f of camera) media.sendVideoFrame(f);
    const t0 = Date.now();
    for (let i = 0; i < 100; i++) {
      const seq = (0xffd0 + i) & 0xffff; // wraps: ROC 1 halfway through
      const roc = 0xffd0 + i > 0xffff ? 1 : 0;
      const hdr = rtp.buildAudioRtpHeader({ seq, ts: 320 * i, ssrc: PEER, twSeq: i });
      const pkt = srtp.protectRtp(hdr, frames[i % frames.length], keys, seq, PEER, roc);
      if (i !== 30) server.send(Buffer.concat([Buffer.from([4]), pkt]), client.port, client.address);
      const wait = t0 + (i + 1) * 20 - Date.now();
      if (wait > 0) await new Promise((r) => setTimeout(r, wait));
    }
    await new Promise((r) => setTimeout(r, 1500));
    const end = z.buildControlHeader({ msgType: 2, cmd: 3, uid: LOCAL, token: TOKEN });
    const cid = Buffer.alloc(4); cid.writeUInt32LE(CALL, 0);
    server.send(Buffer.concat([end, cid]), client.port, client.address);
    await new Promise((r) => setTimeout(r, 300));
    const stats = media.stats();
    media.stop();
    await new Promise((r) => setTimeout(r, 1500));

    assert.deepStrictEqual(events, [['keyframe', 'start'], ['peerEnd']]);
    // Video from the phone reaches the engine as RTP payloads, the missing one NACKed.
    assert.strictEqual(video.length, 29);
    assert.ok(video.every((v) => v.ssrc === PEER && Buffer.isBuffer(v.payload)));
    assert.deepStrictEqual(Buffer.concat(video.slice(0, 3).map((v) => v.payload.subarray(2))), phoneFrames[0]);
    assert.ok(seen.nack >= 1, 'NACK for the lost video packet');
    // Our camera reaches the relay whole: the key frame and the deltas after it.
    assert.deepStrictEqual(seen.upFrames, camera.slice(1).map((f) => f.data));
    assert.strictEqual(stats.rx, 99, 'every downlink packet but the dropped one, across the ROC wrap');
    assert.strictEqual(stats.rxBad, 0);
    assert.ok(seen.up >= 50 && seen.upBad === 0, `uplink SRTP audio decrypts with the session keys (${seen.up} ok, ${seen.upBad} bad)`);
    assert.ok(seen.twcc > 0, 'RTCP (transport-cc / SR) goes out');
    assert.ok(seen.p2pBind > 0, 'P2P binding to the peer candidate');
    assert.strictEqual(seen.endCall, 1, 'EndCall once at stop');
    const log = logs.join('\n');
    for (const line of ['media: negotiated { servers: 1, srtpMode: 1, sendUdp: true }', 'media: started', 'media: first audio from peer',
      'media: EndCall from the active server', 'media: zcall-audio exited 0', 'media: first video from peer (PT 98)']) {
      assert.ok(log.includes(line), `log has "${line}"`);
    }
    assert.match(log, /media: stopped \{"tx":\d+,"rx":99,"rxBad":0,/);
    assert.ok(fs.statSync(path.join(outDir, 'played.raw')).size > 100 * 640, 'zcall-audio played the downlink');
  });
});
