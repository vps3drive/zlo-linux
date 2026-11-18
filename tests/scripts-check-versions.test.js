'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const http = require('http');
const https = require('https');
const { getLatestZaloVersion, main } = require('../scripts/check-versions');

// A local server answering each request with the next behaviour in `plan`:
// 'hang' (never answers), 'redirect', or an HTTP status number.
async function server(plan) {
  const seen = [];
  const srv = http.createServer((req, res) => {
    const what = plan[Math.min(seen.length, plan.length - 1)];
    seen.push(what);
    if (what === 'hang') return;
    if (what === 'redirect') {
      res.writeHead(302, { Location: 'https://res-download-pc.zadn.vn/mac/ZaloSetup-universal-26.11.10.dmg' });
      res.end();
      return;
    }
    res.writeHead(what);
    res.end();
  });
  await new Promise((resolve) => srv.listen(0, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${srv.address().port}/`, seen, close: () => { srv.closeAllConnections(); srv.close(); } };
}

describe('check-versions', () => {
  it('retries a timeout and gives up with an error after the last attempt', async () => {
    const s = await server(['hang']);
    try {
      await assert.rejects(getLatestZaloVersion({ url: s.url, timeoutMs: 100, delays: [10, 10] }), /Request timeout/);
      assert.strictEqual(s.seen.length, 3);
    } finally {
      s.close();
    }
  });

  it('recovers when a retry succeeds', async () => {
    const s = await server(['hang', 503, 'redirect']);
    try {
      assert.strictEqual(await getLatestZaloVersion({ url: s.url, timeoutMs: 100, delays: [10, 10] }), '26.11.10');
      assert.deepStrictEqual(s.seen, ['hang', 503, 'redirect']);
    } finally {
      s.close();
    }
  });

  it('does not retry an answer that will not change (HTTP 404)', async () => {
    const s = await server([404]);
    try {
      await assert.rejects(getLatestZaloVersion({ url: s.url, timeoutMs: 100, delays: [10, 10] }), /Unexpected HTTP 404/);
      assert.strictEqual(s.seen.length, 1);
    } finally {
      s.close();
    }
  });

  it('main() rejects instead of exiting 0 when the version lookup fails', async (t) => {
    // zalo.me answers 404: no retry, so this needs no network.
    t.mock.method(https, 'get', (_url, _opts, onResponse) => {
      setImmediate(() => onResponse({ statusCode: 404, headers: {}, destroy() {} }));
      return { on() { return this; }, setTimeout() { return this; }, destroy() {} };
    });
    t.mock.method(process, 'exit', (code) => { throw new Error(`process.exit(${code}) called`); });
    const saved = process.env.ZALO_VERSION;
    delete process.env.ZALO_VERSION;
    try {
      await assert.rejects(main(), /Unexpected HTTP 404/);
    } finally {
      if (saved !== undefined) process.env.ZALO_VERSION = saved;
    }
  });
});
