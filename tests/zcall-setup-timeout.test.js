'use strict';

// Outgoing 1-1 call: how long the engine waits for the phone's ring_ring
// before it gives up with "unreachable". A sleeping phone took 10 s to ring;
// 15 s cut calls that were still ringing, so the default is 45 s.

const { describe, it, mock } = require('node:test');
const assert = require('node:assert/strict');
const path = require('path');
const { ZCALL_DIR } = require('../scripts/utils/paths');

const ENGINE = path.join(ZCALL_DIR, 'native-engine', 'engine-core');

function outgoingCall() {
  const { EngineCore } = require(ENGINE);
  const frames = [];
  const logs = [];
  const ui = { open() {}, status() {}, close() {}, onHangup() {}, incoming() {} };
  const media = { stop() {}, prepare: async () => null, start() {} };
  const engine = new EngineCore({ emit: (f) => frames.push(f), media, log: (...a) => logs.push(a.join(' ')), ui });
  engine.onZaloFrame({ type: 'request', command: 'makeCall', data: { type: 1, partner: [{ id: '1234567890123456789', name: 'Peer' }] } });
  const gaveUp = () => frames.some((f) => f.type === 'sendSignal' && f.command === 405);
  return { engine, logs, gaveUp };
}

describe('outgoing call: wait for ring_ring', () => {
  it('waits 45 s by default, not 15 s', (t) => {
    mock.timers.enable({ apis: ['setTimeout'] });
    t.after(() => mock.timers.reset());
    const { logs, gaveUp } = outgoingCall();
    mock.timers.tick(15000);
    assert.ok(!gaveUp(), 'still calling after 15 s');
    mock.timers.tick(29999);
    assert.ok(!gaveUp(), 'still calling just before 45 s');
    mock.timers.tick(1);
    assert.ok(gaveUp(), '405 at 45 s');
    assert.ok(logs.some((l) => l.includes('unreachable / no network (timeout)')));
  });

  it('keeps ZCALL_SETUP_TIMEOUT_MS', (t) => {
    const saved = process.env.ZCALL_SETUP_TIMEOUT_MS;
    process.env.ZCALL_SETUP_TIMEOUT_MS = '5000';
    delete require.cache[require.resolve(ENGINE)];
    mock.timers.enable({ apis: ['setTimeout'] });
    t.after(() => {
      mock.timers.reset();
      if (saved === undefined) delete process.env.ZCALL_SETUP_TIMEOUT_MS;
      else process.env.ZCALL_SETUP_TIMEOUT_MS = saved;
      delete require.cache[require.resolve(ENGINE)];
    });
    const { gaveUp } = outgoingCall();
    mock.timers.tick(4999);
    assert.ok(!gaveUp());
    mock.timers.tick(1);
    assert.ok(gaveUp(), '405 at ZCALL_SETUP_TIMEOUT_MS');
  });
});
