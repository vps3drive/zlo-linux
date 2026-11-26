'use strict';

// Tells the user when a newer AppImage is published on the release repository
// this build came from. It only notifies and links to the release page: it
// never downloads or replaces anything.

const fs = require('fs');
const path = require('path');

const ENV_DISABLE = 'ZALO_UPDATE_CHECK';
const FIRST_CHECK_DELAY_MS = 30 * 1000;
const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
const REQUEST_TIMEOUT_MS = 20 * 1000;
const SETTINGS_KEY = 'updateCheckNotified';

function readBuildInfo(appDir) {
  try {
    const info = JSON.parse(fs.readFileSync(path.join(appDir, 'pc-dist', 'build-info.json'), 'utf8'));
    if (!info || typeof info !== 'object') return null;
    if (typeof info.version !== 'string' || typeof info.commit !== 'string' || !info.commit) return null;
    if (typeof info.releaseRepo !== 'string' || !/^[\w.-]+\/[\w.-]+$/.test(info.releaseRepo)) return null;
    return info;
  } catch (_) {
    return null;
  }
}

function archSuffix(arch = process.arch) {
  if (arch === 'x64') return 'x86_64';
  if (arch === 'arm64') return 'aarch64';
  return null;
}

// Artifact names come from scripts/build.js:
//   Zalo-<ver>-Original-<commit>-<arch>.AppImage
//   Zalo-<ver>+ZaDark-<zadark>-<commit>-<arch>.AppImage
function assetPattern(variant, arch) {
  const middle = variant === 'zadark' ? '\\+ZaDark-[^-]+' : '-Original';
  return new RegExp(`^Zalo-(\\d+(?:\\.\\d+)*)${middle}-([0-9a-f]{7,40})-${arch}\\.AppImage$`);
}

function compareVersions(a, b) {
  const pa = String(a).split('.').map(Number);
  const pb = String(b).split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d !== 0) return d > 0 ? 1 : -1;
  }
  return 0;
}

/**
 * Picks the asset matching this build's variant and architecture from a
 * GitHub release and decides whether it is newer than the running build.
 * Returns null when there is nothing to report.
 */
function findUpdate(release, buildInfo, arch = archSuffix()) {
  if (!release || release.draft || release.prerelease || !Array.isArray(release.assets) || !arch) return null;
  const variant = buildInfo.zadarkVersion ? 'zadark' : 'original';
  const pattern = assetPattern(variant, arch);
  for (const asset of release.assets) {
    const match = asset && typeof asset.name === 'string' && asset.name.match(pattern);
    if (!match) continue;
    const [, version, commit] = match;
    if (commit === buildInfo.commit) return null;
    const order = compareVersions(version, buildInfo.version);
    if (order < 0) return null;
    if (order === 0) {
      // Same Zalo version, different build: only newer if published after we were built.
      const published = Date.parse(release.published_at);
      const built = Date.parse(buildInfo.buildDate);
      if (!(published > built)) return null;
    }
    return {
      version,
      commit,
      assetName: asset.name,
      url: typeof release.html_url === 'string' ? release.html_url : `https://github.com/${buildInfo.releaseRepo}/releases/latest`
    };
  }
  return null;
}

function fetchLatestRelease(repo, net) {
  return new Promise((resolve, reject) => {
    const request = net.request({
      method: 'GET',
      url: `https://api.github.com/repos/${repo}/releases/latest`
    });
    request.setHeader('Accept', 'application/vnd.github+json');
    request.setHeader('User-Agent', 'zalo-for-linux-update-check');
    const timer = setTimeout(() => {
      request.abort();
      reject(new Error('timeout'));
    }, REQUEST_TIMEOUT_MS);
    request.on('response', (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(Buffer.from(chunk)));
      response.on('end', () => {
        clearTimeout(timer);
        if (response.statusCode !== 200) {
          reject(new Error(`HTTP ${response.statusCode}`));
          return;
        }
        try {
          resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
        } catch (error) {
          reject(error);
        }
      });
      response.on('error', (error) => {
        clearTimeout(timer);
        reject(error);
      });
    });
    request.on('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
    request.end();
  });
}

function register({
  app,
  appDir,
  onTrayMenuChanged,
  env = process.env,
  electron = null,
  fetchRelease = null,
  setTimer = setTimeout,
  setRepeat = setInterval
} = {}) {
  if (!app || typeof app.getPath !== 'function') {
    throw new TypeError('Update check requires an Electron app instance.');
  }

  const messages = require('./locale.json').messages;
  let currentLocale = 'vi';
  let available = null;
  let timers = [];

  const translate = (key, vars = {}) => {
    const entry = messages['update-check.' + key];
    const text = entry ? entry[currentLocale] : key;
    return text.replace(/\{(\w+)\}/g, (_, name) => (name in vars ? String(vars[name]) : ''));
  };

  function setLocale(locale) {
    if (locale !== 'vi' && locale !== 'en') return false;
    currentLocale = locale;
    return true;
  }

  const getElectron = () => electron || require('electron');
  const buildInfo = appDir ? readBuildInfo(appDir) : null;
  // Only an AppImage is updated from these releases; Flatpak, Nix and source
  // runs get their updates elsewhere.
  const enabled = Boolean(buildInfo) && Boolean(env.APPIMAGE) && env[ENV_DISABLE] !== '0';

  const settingsPath = path.join(app.getPath('userData'), 'zalo-linux-settings.json');
  function readSettings() {
    try {
      const settings = JSON.parse(fs.readFileSync(settingsPath, 'utf8'));
      return settings && typeof settings === 'object' && !Array.isArray(settings) ? settings : {};
    } catch (_) {
      return {};
    }
  }
  function rememberNotified(key) {
    try {
      const settings = readSettings();
      fs.mkdirSync(path.dirname(settingsPath), { recursive: true });
      const temporary = settingsPath + '.tmp';
      fs.writeFileSync(temporary, JSON.stringify({ ...settings, [SETTINGS_KEY]: key }, null, 2), { mode: 0o600 });
      fs.renameSync(temporary, settingsPath);
    } catch (error) {
      console.error('[update-check] Cannot save settings:', error.message);
    }
  }

  function openReleasePage() {
    if (!available) return;
    try {
      getElectron().shell.openExternal(available.url);
    } catch (error) {
      console.error('[update-check] Cannot open release page:', error);
    }
  }

  function notify(update) {
    const key = update.assetName;
    if (readSettings()[SETTINGS_KEY] === key) return false;
    const { Notification } = getElectron();
    if (!Notification || (typeof Notification.isSupported === 'function' && !Notification.isSupported())) return false;
    const notification = new Notification({
      title: translate('title'),
      body: translate('body', { version: update.version, commit: update.commit })
    });
    notification.on('click', openReleasePage);
    notification.show();
    rememberNotified(key);
    return true;
  }

  async function check() {
    if (!enabled) return null;
    try {
      const release = fetchRelease
        ? await fetchRelease(buildInfo.releaseRepo)
        : await fetchLatestRelease(buildInfo.releaseRepo, getElectron().net);
      const update = findUpdate(release, buildInfo);
      const changed = (update && update.assetName) !== (available && available.assetName);
      available = update;
      if (changed && typeof onTrayMenuChanged === 'function') onTrayMenuChanged();
      if (update) notify(update);
      return update;
    } catch (error) {
      console.warn('[update-check] Check failed:', error.message);
      return null;
    }
  }

  if (enabled) {
    timers.push(setTimer(check, FIRST_CHECK_DELAY_MS));
    timers.push(setRepeat(check, CHECK_INTERVAL_MS));
    for (const timer of timers) if (timer && typeof timer.unref === 'function') timer.unref();
  }

  return {
    isEnabled: () => enabled,
    check,
    getAvailable: () => available,
    getTrayMenuItems() {
      if (!available) return [];
      return [{
        label: translate('trayItem', { version: available.version, commit: available.commit }),
        click: openReleasePage
      }];
    },
    setLocale,
    onLanguageUpdateRequest(request) {
      if (request && setLocale(request.locale) && available && typeof onTrayMenuChanged === 'function') {
        onTrayMenuChanged();
      }
    },
    dispose() {
      for (const timer of timers) {
        clearTimeout(timer);
        clearInterval(timer);
      }
      timers = [];
    }
  };
}

module.exports = { register, findUpdate, readBuildInfo, compareVersions };
