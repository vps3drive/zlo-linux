'use strict';
// Runs the JS engine's media modules on deterministic scenarios and writes
// what they produce to vectors.json; tests/vectors.rs replays the same inputs
// through the Rust port and must get the same bytes.
//
//   node native/engines/zcall-media/tests/gen-vectors.js
//
// The JS modules are the reference while both exist: regenerate after
// changing either side, and a diff in vectors.json is a behaviour change.

const fs = require('fs');
const path = require('path');

const ENGINE = path.join(__dirname, '..', '..', 'zcall', 'native-engine');
delete process.env.ZCALL_SFU_BYTE0;
delete process.env.ZCALL_GROUP_CAM_LAYER;
delete process.env.ZCALL_VIDEO_RUNG;
delete process.env.ZCALL_VIDEO_ADAPT;
const srtp = require(path.join(ENGINE, 'srtp-aes'));
const rtp = require(path.join(ENGINE, 'rtp'));
const z = require(path.join(ENGINE, 'zrtp-packet'));
const rtcp = require(path.join(ENGINE, 'rtcp'));
const { SendRate } = require(path.join(ENGINE, 'send-rate'));

// mulberry32
function prng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rnd = prng(20261118);
const int = (n) => Math.floor(rnd() * n);
const bytes = (n) => Buffer.from(Array.from({ length: n }, () => int(256)));
const hex = (b) => (b ? Buffer.from(b).toString('hex') : null);

const out = {};

// --- SRTP --------------------------------------------------------------------
out.srtp = [];
for (let k = 0; k < 6; k++) {
  const master = bytes(30 + int(10));
  const keys = srtp.sessionKeys(srtp.masterFromSessId(master));
  const packets = [];
  for (let i = 0; i < 12; i++) {
    const seq = [0, 1, 0x7fff, 0xffff][i % 4] ^ int(4);
    const ssrc = int(0x100000000) >>> 0;
    const roc = [0, 1, 7, 65535][int(4)];
    const header = rtp.buildAudioRtpHeader({ seq, ts: int(0x100000000), ssrc, twSeq: int(0x10000) });
    const payload = bytes(int(200));
    const prot = srtp.protectRtp(header, payload, keys, seq, ssrc, roc);
    const tampered = Buffer.from(prot); tampered[tampered.length - 1 - int(5)] ^= 1;
    const un = srtp.unprotectRtp(prot, keys, roc);
    packets.push({
      header: hex(header), payload: hex(payload), seq, ssrc, roc,
      iv: hex(srtp.packetIv(keys.salt, ssrc, seq, roc)),
      protected: hex(prot), unprotected: un ? hex(un.opus) : null,
      tampered: hex(tampered), tamperedOk: srtp.unprotectRtp(tampered, keys, roc) !== null,
      wrongRocOk: srtp.unprotectRtp(prot, keys, roc + 1) !== null,
    });
  }
  out.srtp.push({ master: hex(master), enc: hex(keys.enc), auth: hex(keys.auth), salt: hex(keys.salt), packets });
}
out.parseRtp = [];
for (let i = 0; i < 20; i++) {
  const b = bytes(int(40));
  if (b.length) b[0] = (b[0] & 0x0f) | (i % 2 ? 0x90 : 0x80);
  const p = srtp.parseRtp(b);
  out.parseRtp.push({ buf: hex(b), hdrLen: p ? p.hdr.length : null });
}

// --- RTP headers -------------------------------------------------------------
out.rtp = [];
for (let i = 0; i < 20; i++) {
  const a = { seq: int(0x10000), ts: int(0x100000000), ssrc: int(0x100000000), twSeq: int(0x10000), marker: !!int(2), pt: [112, 97, 98][int(3)] };
  const captureMs = 1.7e12 + rnd() * 1e10;
  const sendMs = captureMs + rnd() * 50;
  const level = int(140);
  const sfu = [null, { id: 12, long: false }, { id: 14, long: true }, { long: true }, {}][int(5)];
  out.rtp.push({
    ...a, captureMs, sendMs, level, sfu, layerSeq: int(0x10000),
    audio: hex(rtp.buildAudioRtpHeader(a)),
    groupAudio: hex(rtp.buildGroupAudioRtpHeader({ ...a, captureMs, level })),
    groupVideo: hex(rtp.buildGroupVideoRtpHeader({ ...a, sfu, layerSeq: out.rtp.length ? int(0x10000) : 0, captureMs, sendMs })),
  });
  // layerSeq must be what was passed: rebuild with the stored one.
  const last = out.rtp[out.rtp.length - 1];
  last.groupVideo = hex(rtp.buildGroupVideoRtpHeader({ ...a, sfu, layerSeq: last.layerSeq, captureMs, sendMs }));
}

// --- ZRTP packets ------------------------------------------------------------
const sess = () => bytes(int(160)).toString('latin1');
out.zrtp = [];
for (let i = 0; i < 12; i++) {
  const uid = int(0x100000000), token = int(0x100000000), callId = int(0x100000000), peerUid = int(0x100000000), seq = int(0x100000000);
  const s = Buffer.from(sess(), 'latin1');
  const extra = int(2) ? bytes(int(6)) : Buffer.alloc(0);
  const layout = z.GROUP_INIT_LAYOUTS[i % 4];
  const host = !!int(2), videoOn = !!int(2), hostCall = int(0x100000000);
  const entries = Array.from({ length: int(4) }, () => ({ uid: int(0x100000000), qualityId: int(256) }));
  const layers = Array.from({ length: int(4) }, () => ({ id: int(9), a: int(2000), b: int(1920), c: int(1080) }));
  const role = int(2) ? 'callee' : 'caller';
  const req = Buffer.concat([Buffer.alloc(9), bytes(10)]);
  out.zrtp.push({
    uid, token, callId, peerUid, seq, sess: hex(s), extra: hex(extra), layout, host, videoOn, hostCall, entries, layers, role, req: hex(req),
    rxs: int(1000), a: int(100), b5: int(10), subCmd: int(256), msgType: [3, 5, 7, 13, 15][int(5)], payload: hex(bytes(int(30))),
    media: null, init: null, group: null, shareInit: null, sharePing: null, zavi: null, ping: null, end: null, fwd: null, fwdStatus: null, vqReq: null, vqCfg: null, sync: null,
    p2pBind: null, p2pAnswer: null, p2pPong: null, p2pAudio: null, p2pVideo: null, p2pRtcpA: null, p2pRtcpV: null,
  });
  const v = out.zrtp[out.zrtp.length - 1];
  v.media = hex(z.wrapMediaPacket({ msgType: v.msgType, token, payload: Buffer.from(v.payload, 'hex') }));
  v.init = hex(z.buildInitZrtpPacket({ role, uid, peerUid, callId, sessId: s, extra, subCmd: 2 }));
  v.group = hex(z.buildGroupInitZrtpPacket({ host, uid, sessId: s, videoOn, callId, hostCall, layout }));
  v.shareInit = hex(z.buildShareInitZrtpPacket({ uid, sessId: s }));
  v.sharePing = hex(z.buildShareZaviPingPacket({ uid, token, sessId: s }));
  v.zavi = hex(z.buildZaviPingPacket({ uid, token, sessId: s, videoOn }));
  v.ping = hex(z.buildPingPacket({ uid, token, sessId: s }));
  v.end = hex(z.buildEndCallPacket({ uid, token, callId }));
  v.fwd = hex(z.buildReqForwardPacket({ uid, token, seq, subCmd: v.subCmd, payload: Buffer.from(v.payload, 'hex') }));
  v.fwdStatus = hex(z.reqForwardStatus(v.rxs, v.a, v.b5));
  v.vqReq = hex(z.buildGroupVidQualReq({ uid, token, seq, entries }));
  v.vqCfg = hex(z.buildGroupVidQualConfig({ uid, token, seq, layers }));
  v.sync = hex(z.buildGroupSyncPacket({ uid, token, seq, subCmd: v.subCmd }));
  const p2pSeq = int(0x10000), index = int(8);
  Object.assign(v, { p2pSeq, index, peerAddr: `10.0.${int(256)}.${int(256)}|${int(65536)}` });
  v.p2pBind = hex(z.buildP2pBind({ role, callId, seq: p2pSeq, index }));
  v.p2pAnswer = hex(z.buildP2pBindAnswer({ role, callId, request: req, peerAddr: v.peerAddr }));
  v.p2pPong = hex(z.buildP2pPong({ role, ping: req }));
  v.p2pAudio = hex(z.wrapP2pAudio({ role, callId, rtp: Buffer.from(v.payload, 'hex') }));
  v.p2pVideo = hex(z.wrapP2pVideo({ role, callId, rtp: Buffer.from(v.payload, 'hex') }));
  v.p2pRtcpA = hex(z.wrapP2pRtcp({ role, callId, video: false, rtcp: Buffer.from(v.payload, 'hex') }));
  v.p2pRtcpV = hex(z.wrapP2pRtcp({ role, callId, video: true, rtcp: Buffer.from(v.payload, 'hex') }));
}

// unwrapPacket: real shapes (built above, plus server replies) and noise.
const toPlain = (p) => {
  if (!p) return null;
  const o = {};
  for (const [k, val] of Object.entries(p)) {
    if (k === 'raw') continue;
    o[k] = Buffer.isBuffer(val) ? hex(val) : val;
  }
  return o;
};
out.unwrap = [];
const replies = [];
for (let i = 0; i < 8; i++) {
  const h = z.buildControlHeader({ msgType: 2, cmd: [11, 12, 2, 3, 32, 50][i % 6], subCmd: int(8), uid: int(1e9), token: int(1e9) });
  const addr = Buffer.from(`1.2.3.${i}|${4000 + i}`, 'latin1');
  const body = [
    Buffer.concat([Buffer.from([0, 0, 0, 0]), bytes(8), Buffer.from([addr.length, 0]), addr]),
    Buffer.from([1, 0, 0, 0]),
    bytes(12),
    Buffer.concat([bytes(8), Buffer.from([5, 0]), bytes(5), Buffer.from([2, 0]), bytes(2)]),
  ][i % 4];
  replies.push(Buffer.concat([h, body]));
}
const samples = [...replies, ...out.zrtp.flatMap((v) => [v.media, v.init, v.ping, v.end, v.fwd, v.p2pBind].map((x) => Buffer.from(x, 'hex'))),
  Buffer.from([4, 1, 2, 3]), Buffer.from([0x0e]), Buffer.from([3, 1]), Buffer.from([0x99, 1]), Buffer.from([2, 0x7e, 0]),
  ...Array.from({ length: 10 }, () => bytes(1 + int(40)))];
for (const b of samples) out.unwrap.push({ buf: hex(b), parsed: toPlain(z.unwrapPacket(b)) });
out.roomUpdated = [bytes(8), bytes(7), Buffer.alloc(0)].map((b) => ({ buf: hex(b), uids: z.parseRoomUpdated(b) }));

// groupVideoPlan
const plans = [
  null, {}, { groupcall: {} }, { groupcall: { temporalNum: 0, spatialLayers: [] } },
  { maxWidth: 1280, maxHeight: 720, groupcall: { codec: 60, temporalNum: 3, spatialLayers: [
    { factorNum: 3, factorDen: 8, bitrateKbps: 100, fps: 12 }, { factorNum: 9, factorDen: 16, bitrateKbps: 300, fps: 12 }, { factorNum: 3, factorDen: 4, bitrateKbps: 700, fps: 15 }] } },
  { maxWidth: '960', keyFrameIntervalMs: 2000, groupcall: { codec: 4, temporalNum: '2', spatialLayers: [{ factorNum: 1, factorDen: 2, bitrateKbps: 250.7, fps: 15.9 }, null, { factorNum: 1, factorDen: 0, bitrateKbps: 1, fps: 1 }] } },
  { groupcall: { codec: 1, temporalNum: 1, spatialLayers: [{ factorNum: 1, factorDen: 1, bitrateKbps: 1500, fps: 30 }, { factorNum: 'x', factorDen: 1 }] } },
  { maxHeight: -5, groupcall: { codec: 61, temporalNum: 2, spatialLayers: [{ factorNum: 1, factorDen: 4, bitrateKbps: 80, fps: 0 }, { factorNum: 1, factorDen: 4, bitrateKbps: 80, fps: 10 }] } },
];
out.plans = [];
for (const cfg of plans) for (const cam of [undefined, '0', '2', 'abc', '-3']) {
  if (cam === undefined) delete process.env.ZCALL_GROUP_CAM_LAYER; else process.env.ZCALL_GROUP_CAM_LAYER = cam;
  out.plans.push({ config: cfg, cam: cam === undefined ? null : cam, plan: z.groupVideoPlan(cfg) });
}
delete process.env.ZCALL_GROUP_CAM_LAYER;

// --- RTCP --------------------------------------------------------------------
// One call's worth of receiving: audio + video RTP with transport-wide seqs,
// relay + P2P duplicates, loss, reordering, a seq wrap; SRs from the phone; the
// engine's timers asking for TWCC (50 ms), reports (1 s) and PLIs.
function rtpPacket({ seq, ts, ssrc, tw, pt }) {
  const h = rtp.buildAudioRtpHeader({ seq, ts, ssrc, twSeq: tw, pt });
  return Buffer.concat([h, bytes(4)]);
}
out.rtcpCalls = [];
for (let call = 0; call < 3; call++) {
  const local = int(0x100000000), peer = int(0x100000000);
  const fb = new rtcp.RtcpFeedback({ localSsrc: local });
  const ops = [];
  let aSeq = call === 1 ? 0xfff0 : int(0x10000), vSeq = int(0x10000), tw = call === 2 ? 0xffe0 : int(0x10000);
  let t = 1.7e12 + int(1e6);
  let ats = int(1e9), vts = int(1e9);
  for (let step = 0; step < 600; step++) {
    t += 5 + rnd() * 10;
    const r = rnd();
    if (r < 0.55) {
      const video = rnd() < 0.4;
      const lost = rnd() < 0.05;
      const pkt = rtpPacket({ seq: video ? vSeq : aSeq, ts: video ? vts : ats, ssrc: peer, tw, pt: video ? 98 : 112 });
      if (video) { vSeq = (vSeq + 1) & 0xffff; vts = (vts + 3000) >>> 0; } else { aSeq = (aSeq + 1) & 0xffff; ats = (ats + 320) >>> 0; }
      tw = (tw + 1) & 0xffff;
      if (lost) continue;
      const late = rnd() < 0.05 ? 1 : 0;
      ops.push({ op: 'rtp', kind: video ? 'video' : 'audio', pkt: hex(pkt), t: t + late * 30 });
      fb.onRtp(video ? 'video' : 'audio', pkt, t + late * 30);
      if (rnd() < 0.1) { ops.push({ op: 'rtp', kind: video ? 'video' : 'audio', pkt: hex(pkt), t: t + 1 }); fb.onRtp(video ? 'video' : 'audio', pkt, t + 1); }
    } else if (r < 0.75) {
      const pk = fb.twccPackets();
      ops.push({ op: 'twcc', t, out: pk.map(hex), parsed: pk.map((p) => rtcp.parseTwcc(p)) });
    } else if (r < 0.85) {
      const rep = fb.audioReport(t, { rtpTs: int(0x100000000), packets: int(1e6), octets: int(1e8) });
      ops.push({ op: 'areport', t, rtpTs: rep.readUInt32BE(16), packets: rep.readUInt32BE(20), octets: rep.readUInt32BE(24), out: hex(rep) });
    } else if (r < 0.92) {
      const rep = fb.videoReport(t);
      ops.push({ op: 'vreport', t, out: hex(rep) });
    } else if (r < 0.96) {
      const sr = rtcp.buildSr({ ssrc: peer, nowMs: t - 20, rtpTs: int(1e9), packets: int(1e5), octets: int(1e7), blocks: [] });
      const compound = Buffer.concat([sr, rtcp.buildSdes(peer)]);
      const kind = rnd() < 0.5 ? 'audio' : 'video';
      ops.push({ op: 'rtcp', kind, t, buf: hex(compound) });
      fb.onRtcp(kind, compound, t);
    } else {
      const p = fb.pli();
      ops.push({ op: 'pli', t, out: hex(p) });
    }
  }
  out.rtcpCalls.push({ local, ops, counters: fb.counters, sent: fb.twcc.sent });
}

// NACK tracker: a video stream with holes, late fills, a seek and a wrap.
out.nack = [];
{
  const tr = new rtcp.NackTracker();
  let seq = 0xffc0;
  let t = 1000;
  for (let i = 0; i < 800; i++) {
    t += rnd() * 15;
    const ssrc = i % 7 === 0 ? 2 : 1;
    let s;
    const r = rnd();
    if (r < 0.75) { seq = (seq + 1) & 0xffff; s = seq; }
    else if (r < 0.88) { seq = (seq + 2 + int(6)) & 0xffff; s = seq; }
    else if (r < 0.95) { s = (seq - int(10)) & 0xffff; }
    else if (r < 0.97) { seq = (seq + 100 + int(500)) & 0xffff; s = seq; }
    else { s = (seq + 0x9000) & 0xffff; }
    const res = tr.note(ssrc, s, t);
    out.nack.push({ ssrc, seq: s, t, out: res, pkts: res.map((x) => hex(rtcp.buildNack(7, x.ssrc, x.seqs))) });
  }
}
out.buildNack = [[1, 2, 3], [65535, 0, 1, 17, 18], [5], [10, 30, 31, 47], []].map((s) => ({ seqs: s, out: hex(rtcp.buildNack(1, 2, s)) }));
out.readTwSeq = [];
for (let i = 0; i < 30; i++) {
  const b = i < 10 ? rtpPacket({ seq: 1, ts: 2, ssrc: 3, tw: int(0x10000), pt: 112 })
    : Buffer.from(rtp.buildGroupVideoRtpHeader({ seq: 1, ts: 2, ssrc: 3, twSeq: int(0x10000), pt: 98, sfu: { id: 12 }, captureMs: 1, sendMs: 1 }));
  if (i >= 20) { b[16 + int(b.length - 16)] = int(256); }
  out.readTwSeq.push({ buf: hex(b), tw: rtcp.readTwSeq(b) });
}

// --- send rate ---------------------------------------------------------------
const configs = [
  {},
  { bwProfiles: [{ id: 1, res: 240, fps: 8, bitrate: 30 }, { id: 3, res: 360, fps: 15, bitrate: 300 }, { id: 'x7', res: 720, fps: 30, bitrate: 1100 }, { id: 5, res: 360, fps: 24, bitrate: 700 }, { id: 9, res: 1080, bitrate: 3000 }],
    codecMaxFps: 24, maxBitrate: 1500, startupBitrate: 500, bweSendSideHighLoss: 7, keyFrameIntervalMs: 0 },
  { bwProfiles: [{ id: 1, res: 240, bitrate: 30 }], maxWidth: 640 },
  { maxBitrate: '900', startupBitrate: 'abc', bweSendSideLowLoss: 3 },
];
const groupRows = z.groupVideoPlan(plans[4]);
out.sendRate = [];
for (let k = 0; k < configs.length + 1; k++) {
  const group = k === configs.length;
  const config = group ? plans[4] : configs[k];
  const local = int(0x100000000);
  let now = 1.7e12;
  const logs = [], profiles = [];
  const env = k === 1 ? 'x7' : null;
  if (env) process.env.ZCALL_VIDEO_RUNG = env; else delete process.env.ZCALL_VIDEO_RUNG;
  const sr = new SendRate(config, {
    localSsrc: local, log: (l) => logs.push(l), onProfile: (p) => profiles.push(p), now: () => now,
    ...(group ? { layers: groupRows.rows, startIndex: groupRows.start } : {}),
  });
  delete process.env.ZCALL_VIDEO_RUNG;
  const ops = [];
  let tw = int(0x10000);
  const phase = (sec) => (sec < 20 ? 'clean' : sec < 30 ? 'loss' : sec < 40 ? 'queue' : sec < 50 ? 'silent' : 'clean');
  for (let sec = 0; sec < 75; sec++) {
    const kind = phase(sec);
    const sentHere = [];
    for (let i = 0; i < 60; i++) {
      now += 1000 / 60;
      sr.onSent(tw, now); sentHere.push([tw, now]); ops.push({ op: 'sent', tw, ms: now });
      tw = (tw + 1) & 0xffff;
    }
    if (kind !== 'silent') {
      const base = sentHere[0][0];
      const arrivals = new Map();
      const delay = kind === 'queue' ? 200 + sec * 5 : 30;
      sentHere.forEach(([s, ms], i) => {
        if (kind === 'loss' && rnd() < 0.2) return;
        if (rnd() < 0.01) return;
        arrivals.set(i, Math.round((ms - 1.7e12 + delay + rnd() * 10) * 4));
      });
      const firstIdx = [...arrivals.keys()][0];
      if (firstIdx !== undefined) {
        const m = new Map([...arrivals].map(([i, q]) => [i, q]));
        const fbPkt = rtcp.encodeTwcc({ senderSsrc: 99, mediaSsrc: local, base: 0, end: sentHere.length - 1, arrivals: m, fbCount: sec & 0xff });
        fbPkt.writeUInt16BE(base, 12);
        const rr = rtcp.buildRr({ ssrc: 99, blocks: [{ ssrc: local, fractionLost: int(40), cumLost: 0, extHighSeq: 0, jitter: 0, lsr: 0, dlsr: 0 }] });
        const compound = Buffer.concat([rtcp.buildSr({ ssrc: 99, nowMs: now, rtpTs: 1, packets: 1, octets: 1, blocks: [] }), rr, fbPkt]);
        const kch = rnd() < 0.5 ? 'audio' : 'video';
        sr.onRtcp(kch, compound); ops.push({ op: 'rtcp', kind: kch, buf: hex(compound) });
      }
    } else if (rnd() < 0.5) {
      const rr = rtcp.buildRr({ ssrc: 99, blocks: [{ ssrc: local, fractionLost: int(256), cumLost: 0, extHighSeq: 0, jitter: 0, lsr: 0, dlsr: 0 }] });
      sr.onRtcp('video', rr); ops.push({ op: 'rtcp', kind: 'video', buf: hex(rr) });
    }
    const sending = !(sec >= 60 && sec < 63);
    const downLoss = sec % 9 === 0 ? null : (sec >= 64 && sec < 67 ? 0.15 : rnd() * 0.03);
    sr.tick(sending, downLoss); ops.push({ op: 'tick', sending, downLoss, now });
  }
  out.sendRate.push({ config, group, env, local, start: 1.7e12, ops, logs, profiles, totals: sr.totals, level: sr.level });
}

fs.writeFileSync(path.join(__dirname, 'vectors.json'), JSON.stringify(out));
console.log('vectors.json:', Object.entries(out).map(([k, v]) => `${k} ${v.length}`).join(', '));
