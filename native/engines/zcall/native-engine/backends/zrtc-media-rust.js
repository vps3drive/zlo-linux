'use strict';
// ZCALL_MEDIA=rust: 1-1 call media in the zcall-media process (Rust,
// native/engines/zcall-media) instead of zrtc-media.js. That process owns the
// UDP socket, SRTP, RTCP and send-rate, and runs zcall-audio itself, so audio
// frames no longer pass through this event loop.
//
// Scope: 1-1 calls, audio and video both ways (the phone's video goes to the
// engine's VideoAssembler as before; our camera / 1-1 screen frames go to the
// process, which packetizes, encrypts and paces them). Group calls stay on
// zrtc-media.js (MediaSwitch picks the backend per call). Video payloads
// cross the pipe as base64 inside the JSON lines.
//
// Control is one JSON object per line on the process's stdin / stdout (see
// native/engines/zcall-media/src/main.rs). Its log lines come back as
// {"ev":"log"} and go through this engine's log(), so native-engine-*.log
// has the same lines, prefix and timestamps as with zrtc-media.js.

const fs = require('fs');
const os = require('os');
const path = require('path');
const readline = require('readline');
const util = require('util');
const { EventEmitter } = require('events');
const { spawn } = require('child_process');
const { ZrtcMediaBackend, audioCommand } = require('./zrtc-media');

function findBinary() {
  const candidates = [
    path.join(__dirname, '..', 'zcall-media'),
    path.join(__dirname, '..', '..', '..', '..', '..', 'target', 'release', 'zcall-media'),
  ];
  return candidates.find((p) => fs.existsSync(p)) || null;
}

class RustMediaBackend extends EventEmitter {
  constructor({ log = () => { }, sendUdp = false, bin }) {
    super();
    this.log = log;
    this.sendUdp = !!sendUdp;
    this.bin = bin;
    this.proc = null;
    this.params = null;
    this.group = false;
    this.devices = {}; // { mic, speaker }, kept across calls as zrtc-media.js does
    this.muted = false;
    this.speakerOff = false;
    this.port = 0;
    this.lastRxAt = 0;
    this._stats = null;
    this._prepared = null;
  }

  _send(obj) {
    if (this.proc && this.proc.stdin.writable) this.proc.stdin.write(JSON.stringify(obj) + '\n');
  }

  _spawn() {
    const proc = spawn(this.bin, [], { stdio: ['pipe', 'pipe', 'pipe'] });
    this.proc = proc;
    proc.stdin.on('error', () => { });
    readline.createInterface({ input: proc.stdout }).on('line', (line) => this._onLine(proc, line));
    readline.createInterface({ input: proc.stderr }).on('line', (line) => { if (line.trim()) this.log('media(rust):', line); });
    proc.on('exit', (code, signal) => {
      const current = this.proc === proc;
      // Always said, so a call that ends because the process died shows why
      // (its panic message, if any, is on the lines above as media(rust)).
      this.log('media: zcall-media exited', code === null ? signal : code, current ? '(during the call)' : '(call stopped)');
      if (current) this.proc = null;
      if (this._prepared && current) { this._prepared(null); this._prepared = null; }
      if (current && code !== 0) this.emit('failed', 'media-process');
    });
    proc.on('error', (e) => this.log('media: zcall-media failed to start:', e.message));
  }

  _onLine(proc, line) {
    let m;
    try { m = JSON.parse(line); } catch (_) { this.log('media(rust):', line); return; }
    if (m.ev === 'log') { this.log(m.msg); return; }
    if (proc !== this.proc) return; // a stopped call's process finishing up
    if (m.ev === 'video') {
      this.emit('video', { seq: m.seq, ts: m.ts, pt: m.pt, marker: m.marker, payload: Buffer.from(m.payload, 'base64'), roc: m.roc, ssrc: m.ssrc });
    } else if (m.ev === 'emit') this.emit(m.name, ...(m.args || []));
    else if (m.ev === 'stats') { this._stats = m.stats; this.lastRxAt = m.lastRxAt || 0; }
    else if (m.ev === 'prepared') {
      this.port = m.port || 0;
      if (this._prepared) { this._prepared(this._best(m.best)); this._prepared = null; }
    }
  }

  // The reply prepare() resolves with, shaped as zrtc-media.js's: server.raw is
  // the entry of params.servers, which the engine answers 416 / 12434 with.
  _best(best) {
    if (!best || !best.server) return null;
    const list = Array.isArray(this.params && this.params.servers) ? this.params.servers : [];
    const raw = list[best.server.index];
    if (!raw) this.log('media: zcall-media chose a server that is not in the list', JSON.stringify(best.server));
    return { ...best, server: { host: best.server.host, port: best.server.port, raw, family: 'udp4' } };
  }

  onNegotiated(params) {
    if (this.proc) this.stop();
    this.params = params || {};
    this.group = !!this.params.group;
    this.muted = !!this.params.muted;
    this.speakerOff = !!this.params.speakerOff;
    this._stats = null;
    this.lastRxAt = 0;
    this.port = 0;
    this._spawn();
    this._send({ cmd: 'negotiated', params: this.params, sendUdp: this.sendUdp });
  }

  prepare(role = 'caller') {
    return new Promise((resolve) => {
      this._prepared = resolve;
      this._send({ cmd: 'prepare', role });
    });
  }

  start(role = 'caller', rtpSerIp = null) {
    // Same command line and log lines as zrtc-media.js. The process spawns
    // zcall-audio and writes these lines after its "media: started", where
    // zrtc-media.js has them.
    const lines = [];
    const audio = this.sendUdp ? audioCommand({ ...this, log: (...a) => lines.push(util.format(...a)) }) : null;
    if (audio) audio.logs = lines;
    else lines.forEach((l) => this.log(l));
    if (this.sendUdp && !audio) this.emit('audioFault', 'audio');
    this._send({ cmd: 'start', role, rtpSerIp, audio });
  }

  stop() {
    const proc = this.proc;
    if (!proc) return;
    this._send({ cmd: 'stop' });
    // From here its exit is the planned one, not a failure of the call.
    this.proc = null;
    try { proc.stdin.end(); } catch (_) { }
    // It sends EndCall, logs the call's totals and exits; this is the fallback.
    setTimeout(() => { try { proc.kill(); } catch (_) { } }, 3000).unref();
  }

  stats() { return this._stats; }

  // Our P2P candidates for 416/402, as zrtc-media.js builds them.
  localCandidates(publicAddr) {
    const out = [];
    if (this.port) {
      for (const list of Object.values(os.networkInterfaces())) {
        for (const a of list || []) if (a.family === 'IPv4' && !a.internal) out.push({ ip: a.address, port: this.port, type: 0 });
      }
    }
    if (publicAddr && publicAddr.includes('|')) {
      const [ip, p] = publicAddr.split('|');
      out.push({ ip, port: Number(p), type: 1 });
    }
    return out;
  }

  setMute(on) { this.muted = !!on; this._send({ cmd: 'mute', on: this.muted }); }
  setSpeaker(off) { this.speakerOff = !!off; this._send({ cmd: 'speaker', off: this.speakerOff }); }
  setMicVolume(percent) {
    const pct = Math.max(0, Math.min(100, Math.round(Number(percent))));
    if (Number.isFinite(pct)) this._send({ cmd: 'micVolume', pct });
  }
  setDevice(kind, name) {
    if (kind !== 'mic' && kind !== 'speaker') return;
    this.devices[kind] = name || '';
    this._send({ cmd: 'device', kind, name: name || '' });
  }

  // One encoded frame from the call window (H.264 Annex-B): camera, or the
  // screen in a 1-1 call (same stream, as zrtc-media.js sends it).
  sendVideoFrame({ key, data }) {
    if (!this.proc || !data || !data.length) return;
    // ui-client decodes the window's base64 into a Buffer; it goes on as base64.
    this._send({ cmd: 'videoFrame', key: !!key, data: (Buffer.isBuffer(data) ? data : Buffer.from(data)).toString('base64') });
  }

  requestPeerKeyFrame(why, mediaSsrc) {
    this._send({ cmd: 'keyRequest', why: String(why || ''), ssrc: Number(mediaSsrc) || 0 });
  }

  // Group calls only (they run on zrtc-media.js).
  setGroupCamera() { }
  startShare() { }
  stopShare() { }
  setMemberRenderWidth() { }
}

// One media object for the engine: zcall-media for 1-1 calls, zrtc-media.js
// for group calls (not ported yet). Events of whichever runs the call go out;
// device, mute and speaker choices reach both, so they hold for the next call.
class MediaSwitch extends EventEmitter {
  constructor({ log, sendUdp, bin }) {
    super();
    this.log = log;
    this.js = new ZrtcMediaBackend({ log, sendUdp });
    this.rust = new RustMediaBackend({ log, sendUdp, bin });
    this.cur = this.js;
    for (const b of [this.js, this.rust]) {
      const own = b.emit.bind(b);
      b.emit = (name, ...args) => (b === this.cur ? this.emit(name, ...args) : own(name, ...args));
    }
  }

  get sendUdp() { return this.cur.sendUdp; }
  get params() { return this.cur.params; }
  get lastRxAt() { return this.cur.lastRxAt; }
  get vidAsked() { return this.cur.vidAsked; }
  get videoPlan() { return this.cur.videoPlan; }

  onNegotiated(params) {
    const next = params && params.group ? this.js : this.rust;
    if (next !== this.cur) this.log(`media: ${next === this.rust ? 'zcall-media (rust)' : 'zrtc-media.js'} for this call`);
    this.cur = next;
    return this.cur.onNegotiated(params);
  }

  prepare(...a) { return this.cur.prepare(...a); }
  start(...a) { return this.cur.start(...a); }
  stop(...a) { return this.cur.stop(...a); }
  stats(...a) { return this.cur.stats(...a); }
  localCandidates(...a) { return this.cur.localCandidates(...a); }
  sendVideoFrame(...a) { return this.cur.sendVideoFrame(...a); }
  requestPeerKeyFrame(...a) { return this.cur.requestPeerKeyFrame(...a); }
  setGroupCamera(...a) { return this.cur.setGroupCamera(...a); }
  startShare(...a) { return this.cur.startShare(...a); }
  stopShare(...a) { return this.cur.stopShare(...a); }
  setMemberRenderWidth(...a) { return this.cur.setMemberRenderWidth(...a); }
  setMute(on) { this.js.setMute(on); this.rust.setMute(on); }
  setSpeaker(off) { this.js.setSpeaker(off); this.rust.setSpeaker(off); }
  setMicVolume(pct) { this.cur.setMicVolume(pct); }
  setDevice(kind, name) { this.js.setDevice(kind, name); this.rust.setDevice(kind, name); }
}

// The media object for ZCALL_MEDIA=rust; zrtc-media.js alone when the
// zcall-media binary is missing.
function createRustMedia({ log, sendUdp }) {
  const bin = findBinary();
  if (!bin) {
    log('media: ZCALL_MEDIA=rust but the zcall-media binary is missing; using zrtc-media.js');
    return new ZrtcMediaBackend({ log, sendUdp });
  }
  log(`media: rust media backend ${bin} (1-1 audio and video; group calls on zrtc-media.js)`);
  return new MediaSwitch({ log, sendUdp, bin });
}

module.exports = { RustMediaBackend, MediaSwitch, createRustMedia, findBinary };
