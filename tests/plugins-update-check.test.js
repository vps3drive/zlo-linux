'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { EventEmitter } = require('events');
const updateCheck = require('../plugins/update-check');

const BUILD = {
  version: '26.11.10',
  zadarkVersion: null,
  commit: '1ba60fb',
  buildDate: '2026-11-19T09:40:00.000Z',
  releaseRepo: 'vps3drive/zlo-linux'
};

function release(assets, extra = {}, assetTime = '2026-11-26T08:30:00Z') {
  return {
    html_url: 'https://github.com/vps3drive/zlo-linux/releases/tag/26.11.20',
    published_at: '2026-11-26T08:30:40Z',
    draft: false,
    prerelease: false,
    assets: assets.map((name) => ({ name, created_at: assetTime, updated_at: assetTime })),
    ...extra
  };
}

const ASSETS_2620 = [
  'Zalo-26.11.20+ZaDark-26.3-925ff72-aarch64.AppImage',
  'Zalo-26.11.20+ZaDark-26.3-925ff72-aarch64.AppImage.zsync',
  'Zalo-26.11.20+ZaDark-26.3-925ff72-x86_64.AppImage',
  'Zalo-26.11.20+ZaDark-26.3-925ff72-x86_64.AppImage.zsync',
  'Zalo-26.11.20-Original-925ff72-aarch64.AppImage',
  'Zalo-26.11.20-Original-925ff72-x86_64.AppImage'
];

function makeAppDir(info) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'zalo-update-check-'));
  fs.mkdirSync(path.join(dir, 'app', 'pc-dist'), { recursive: true });
  fs.mkdirSync(path.join(dir, 'user'));
  if (info) fs.writeFileSync(path.join(dir, 'app', 'pc-dist', 'build-info.json'), JSON.stringify(info));
  return dir;
}

function fakeElectron() {
  const shown = [];
  const opened = [];
  class Notification extends EventEmitter {
    constructor(options) { super(); this.options = options; }
    static isSupported() { return true; }
    show() { shown.push(this); }
  }
  return { shown, opened, electron: { Notification, shell: { openExternal: (url) => opened.push(url) } } };
}

function registerWith(dir, { fetchRelease, env = { APPIMAGE: '/opt/Zalo.AppImage' }, onTrayMenuChanged } = {}) {
  const app = { getPath: () => path.join(dir, 'user') };
  const fake = fakeElectron();
  const plugin = updateCheck.register({
    app,
    appDir: path.join(dir, 'app'),
    env,
    electron: fake.electron,
    fetchRelease,
    onTrayMenuChanged,
    setTimer: () => null,
    setRepeat: () => null
  });
  return { plugin, ...fake };
}

describe('update-check plugin', () => {
  it('picks the asset of the same variant and architecture', () => {
    const original = updateCheck.findUpdate(release(ASSETS_2620), BUILD, 'x86_64');
    assert.equal(original.assetName, 'Zalo-26.11.20-Original-925ff72-x86_64.AppImage');
    assert.equal(original.version, '26.11.20');
    assert.equal(original.commit, '925ff72');

    const zadark = updateCheck.findUpdate(release(ASSETS_2620), { ...BUILD, zadarkVersion: '26.3' }, 'aarch64');
    assert.equal(zadark.assetName, 'Zalo-26.11.20+ZaDark-26.3-925ff72-aarch64.AppImage');
  });

  it('reports nothing for the running build, an older Zalo, drafts or a missing arch', () => {
    assert.equal(updateCheck.findUpdate(release(ASSETS_2620), { ...BUILD, version: '26.11.20', commit: '925ff72' }, 'x86_64'), null);
    assert.equal(updateCheck.findUpdate(release(ASSETS_2620), { ...BUILD, version: '26.12.1' }, 'x86_64'), null);
    assert.equal(updateCheck.findUpdate(release(ASSETS_2620, { draft: true }), BUILD, 'x86_64'), null);
    assert.equal(updateCheck.findUpdate(release(ASSETS_2620, { prerelease: true }), BUILD, 'x86_64'), null);
    assert.equal(updateCheck.findUpdate(release(['Zalo-26.11.20-Original-925ff72-x86_64.AppImage']), BUILD, 'aarch64'), null);
  });

  it('treats a rebuild of the same Zalo version as newer only when its asset was uploaded after this build', () => {
    const same = { ...BUILD, version: '26.11.20' };
    assert.ok(updateCheck.findUpdate(release(ASSETS_2620), same, 'x86_64'));
    assert.equal(updateCheck.findUpdate(release(ASSETS_2620, {}, '2026-11-01T00:00:00Z'), same, 'x86_64'), null);

    // The workflow reuses the tag: published_at stays at the first build while
    // the assets are replaced by a later one.
    const built0900 = { ...same, buildDate: '2026-11-26T09:00:00Z' };
    const reused = release(ASSETS_2620, { published_at: '2026-11-26T08:30:40Z' }, '2026-11-26T09:40:00Z');
    assert.ok(updateCheck.findUpdate(reused, built0900, 'x86_64'));
    const older = release(ASSETS_2620, { published_at: '2026-11-26T09:30:00Z' }, '2026-11-26T08:50:00Z');
    assert.equal(updateCheck.findUpdate(older, built0900, 'x86_64'), null);
  });

  it('compares versions numerically', () => {
    assert.equal(updateCheck.compareVersions('26.11.20', '26.11.9'), 1);
    assert.equal(updateCheck.compareVersions('26.11', '26.11.0'), 0);
    assert.equal(updateCheck.compareVersions('25.12.1', '26.1.1'), -1);
  });

  it('stays off outside an AppImage, when disabled, or without release metadata', async () => {
    const dir = makeAppDir(BUILD);
    const noInfo = makeAppDir(null);
    const oldInfo = makeAppDir({ ...BUILD, releaseRepo: undefined });
    try {
      let calls = 0;
      const fetchRelease = async () => { calls++; return release(ASSETS_2620); };
      for (const [d, env] of [[dir, {}], [dir, { APPIMAGE: '/x', ZALO_UPDATE_CHECK: '0' }], [noInfo, { APPIMAGE: '/x' }], [oldInfo, { APPIMAGE: '/x' }]]) {
        const { plugin } = registerWith(d, { fetchRelease, env });
        assert.equal(plugin.isEnabled(), false);
        assert.equal(await plugin.check(), null);
      }
      assert.equal(calls, 0);
    } finally {
      for (const d of [dir, noInfo, oldInfo]) fs.rmSync(d, { recursive: true, force: true });
    }
  });

  it('notifies once per release, adds a tray item and opens the release page', async () => {
    const dir = makeAppDir(BUILD);
    try {
      let trayChanges = 0;
      const arch = process.arch === 'arm64' ? 'aarch64' : 'x86_64';
      const { plugin, shown, opened } = registerWith(dir, {
        fetchRelease: async (repo) => { assert.equal(repo, 'vps3drive/zlo-linux'); return release(ASSETS_2620); },
        onTrayMenuChanged: () => trayChanges++
      });
      assert.deepEqual(plugin.getTrayMenuItems(), []);

      const update = await plugin.check();
      assert.equal(update.assetName, `Zalo-26.11.20-Original-925ff72-${arch}.AppImage`);
      assert.equal(trayChanges, 1);
      assert.equal(shown.length, 1);
      assert.match(shown[0].options.body, /26\.11\.20 \(925ff72\)/);

      const items = plugin.getTrayMenuItems();
      assert.equal(items.length, 1);
      assert.equal(items[0].label, 'Có bản mới: 26.11.20 (925ff72)');
      items[0].click();
      shown[0].emit('click');
      assert.deepEqual(opened, [release([]).html_url, release([]).html_url]);

      await plugin.check();
      assert.equal(shown.length, 1, 'same release must not notify twice');
      assert.equal(trayChanges, 1);

      const settings = JSON.parse(fs.readFileSync(path.join(dir, 'user', 'zalo-linux-settings.json'), 'utf8'));
      assert.equal(settings.updateCheckNotified, update.assetName);

      const again = registerWith(dir, { fetchRelease: async () => release(ASSETS_2620) });
      await again.plugin.check();
      assert.equal(again.shown.length, 0, 'remembered across restarts');
      assert.equal(again.plugin.getTrayMenuItems().length, 1, 'tray item still shown');

      plugin.setLocale('en');
      assert.equal(plugin.getTrayMenuItems()[0].label, 'Update available: 26.11.20 (925ff72)');
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  it('keeps other settings and survives network errors', async () => {
    const dir = makeAppDir(BUILD);
    try {
      fs.writeFileSync(path.join(dir, 'user', 'zalo-linux-settings.json'), JSON.stringify({ devMode: 1 }));
      const failing = registerWith(dir, { fetchRelease: async () => { throw new Error('offline'); } });
      assert.equal(await failing.plugin.check(), null);
      assert.deepEqual(failing.plugin.getTrayMenuItems(), []);

      const ok = registerWith(dir, { fetchRelease: async () => release(ASSETS_2620) });
      await ok.plugin.check();
      const settings = JSON.parse(fs.readFileSync(path.join(dir, 'user', 'zalo-linux-settings.json'), 'utf8'));
      assert.equal(settings.devMode, 1);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
});
