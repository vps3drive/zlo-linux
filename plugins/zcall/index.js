/**
 * plugins/zcall/index.js
 *
 * Zalo calls on the native Linux engine (zcall-native/).
 *
 * The patched main process (scripts/patches/patch-zcall-native.js) spawns
 * the engine when a call starts; this plugin
 *   - tells it where the engine is and how to run (environment, below);
 *   - opens the call window's socket (window.js) before the engine starts;
 *   - checks the system packages on the first call and says how to install
 *     what is missing (global.__zcallPrepare, awaited by the patch);
 *   - ends a call in progress when Zalo quits.
 */

'use strict';

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');
const callWindow = require('./window');
const localeMessages = require('./ui/locale.json').messages;
let currentLocale = 'vi';
const translate = (key) => { const entry = localeMessages['zcall.' + key]; return entry ? entry[currentLocale] : key; };

/** A file of zcall-native/: next to the executable when packaged, else in the checkout. */
function enginePath(...parts) {
 const candidates = [
  path.join(path.dirname(process.execPath), 'zcall-native', ...parts),
  path.join(__dirname, '..', '..', 'native', 'engines', 'zcall', ...parts),
  path.join(__dirname, '..', '..', 'zcall-native', ...parts)
 ];
 return candidates.find((p) => fs.existsSync(p)) || candidates[1];
}

// Zalo's own main-dist/main.js: the engine reads the call transport key from it.
function zaloMainJs() {
 return [
  path.join(path.dirname(process.execPath), 'app', 'main-dist', 'main.js'),
  path.join(__dirname, '..', '..', 'app', 'main-dist', 'main.js')
 ].find((p) => fs.existsSync(p));
}

// Processes running this install's engine or its zcall-audio: an argument is
// exactly one of their files (not just a command line mentioning them).
function engineProcesses() {
 const scripts = [enginePath('native-engine', 'zcall-native.js'), enginePath('native-engine', 'zcall-audio')];
 const found = [];
 for (const pid of fs.readdirSync('/proc').filter((d) => /^\d+$/.test(d)).map(Number)) {
  try {
   const argv = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0');
   if (!argv.some((a) => scripts.includes(a))) continue;
   const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8'); // "pid (comm) state ppid ..."
   const ppid = Number(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[1]);
   found.push({ pid, ppid });
  } catch (_) { /* exited meanwhile */ }
 }
 return found;
}

// Reparented to init / the systemd user manager: its Zalo is gone.
function isOrphan(ppid) {
 if (ppid === 1) return true;
 try { return fs.readFileSync(`/proc/${ppid}/comm`, 'utf8').trim() === 'systemd'; } catch (_) { return true; }
}

function kill(pid, signal) {
 try { process.kill(pid, signal); } catch (_) { /* gone */ }
}

// --- system packages -------------------------------------------------------

// What the engine's audio side (zcall-audio) needs: libopus, and
// libpulse-simple (or at least arecord / aplay). pactl, for the device menu
// and the microphone volume, comes with the PulseAudio / PipeWire tools.
const PACKAGES = {
 debian: { cmd: 'sudo apt install', opus: 'libopus0', audio: 'libpulse0' },
 fedora: { cmd: 'sudo dnf install', opus: 'opus', audio: 'pulseaudio-libs' },
 arch: { cmd: 'sudo pacman -S', opus: 'opus', audio: 'libpulse' },
 suse: { cmd: 'sudo zypper install', opus: 'libopus0', audio: 'libpulse0' }
};

function distroFamily() {
 let ids = '';
 try {
  const rel = fs.readFileSync('/etc/os-release', 'utf8');
  ids = [/^ID=(.*)$/m, /^ID_LIKE=(.*)$/m].map((re) => (rel.match(re) || [])[1] || '').join(' ');
 } catch (_) { /* unknown: Debian names */ }
 ids = ids.replace(/"/g, '').toLowerCase();
 if (/\b(fedora|rhel|centos)\b/.test(ids)) return 'fedora';
 if (/\b(arch|manjaro)\b/.test(ids)) return 'arch';
 if (/\b(suse|opensuse)\b/.test(ids)) return 'suse';
 return 'debian';
}

/** The install command for these needs ('opus' / 'audio'). */
function installHint(needs) {
 const d = PACKAGES[distroFamily()];
 return d.cmd + ' ' + needs.map((n) => d[n]).join(' ');
}

let missingCache;

/** The install command for what is missing, or null. Checked once (zcall-audio --check). */
function missingPackages() {
 if (missingCache !== undefined) return missingCache;
 const res = spawnSync(enginePath('native-engine', 'zcall-audio'), ['--check'], { encoding: 'utf8', timeout: 10000 });
 // No binary (or it would not run) is a packaging problem, not a package to
 // install: let the call start, and the engine reports the missing audio.
 const needs = res.error || res.status !== 0
  ? []
  : res.stdout.trim().split(/\s+/).filter((n) => n in PACKAGES.debian && n !== 'cmd');
 if (res.error || res.status !== 0) console.warn('[zcall] zcall-audio --check failed:', (res.error && res.error.message) || res.stderr);
 missingCache = needs.length ? installHint(needs) : null;
 return missingCache;
}

function notify(body) {
 const { Notification } = require('electron');
 if (Notification.isSupported()) new Notification({ title: 'Zalo', body }).show();
}

// Awaited by the patched main process right before it spawns the engine;
// a rejection cancels this start and Zalo tries again on the next call.
function prepare() {
 const missing = missingPackages();
 if (!missing) return Promise.resolve();
 notify(translate('missingPackages') + ' ' + missing);
 return Promise.reject(new Error('calls need system packages: ' + missing));
}

// --- lifecycle -------------------------------------------------------------

/**
 * Before 'ready', after Zalo's bootstrap: on Wayland, screen sharing needs
 * Chromium's PipeWire capture (through the desktop portal); X11 capture
 * only sees XWayland windows. Chromium keeps the last --enable-features, so
 * merge with Zalo's own (JXL). In Flatpak (without --socket=wayland), WebRTC
 * also needs WAYLAND_DISPLAY to route capture to the desktop portal.
 */
function configure(app) {
 const isWayland = (process.env.XDG_SESSION_TYPE || '').trim().toLowerCase() === 'wayland';
 if (!isWayland) return;
 if (!process.env.WAYLAND_DISPLAY) process.env.WAYLAND_DISPLAY = 'wayland-0';
 const features = app.commandLine.getSwitchValue('enable-features').split(',').filter(Boolean);
 if (!features.includes('WebRTCPipeWireCapturer')) features.push('WebRTCPipeWireCapturer');
 app.commandLine.appendSwitch('enable-features', features.join(','));
}

/** At 'ready', before Zalo can start a call. */
function start({ userDataDir }) {
 // Left over from an unclean exit (another running Zalo keeps its own).
 for (const p of engineProcesses()) if (isOrphan(p.ppid)) kill(p.pid, 'SIGKILL');
 callWindow.start();
 Object.assign(process.env, {
  ZCALL_ENGINE_JS: enginePath('native-engine', 'zcall-native.js'),
  // js (zrtc-media.js) unless ZCALL_MEDIA=rust asks for the zcall-media process.
  ZCALL_MEDIA: process.env.ZCALL_MEDIA === 'rust' ? 'rust' : 'zrtc',
  ZCALL_SEND_UDP: '1',
  ZCALL_LOG_DIR: userDataDir // the AppImage is read-only
 });
 const mainJs = process.env.ZALO_MAIN_JS || zaloMainJs();
 if (mainJs) process.env.ZALO_MAIN_JS = mainJs;
 global.__zcallPrepare = prepare;
}

/**
 * At 'will-quit'. SIGTERM to our engine: it hangs up a call in progress (the
 * phone learns at once) and exits ~300 ms later; its zcall-audio follows. The
 * next start SIGKILLs anything left.
 */
function stop() {
 for (const p of engineProcesses()) if (p.ppid === process.pid) kill(p.pid, 'SIGTERM');
 callWindow.stop();
}

function setLocale(locale) {
 if (locale !== 'vi' && locale !== 'en') return false;
 currentLocale = locale;
 return callWindow.setLocale(locale);
}
function onLanguageUpdateRequest(request) {
 if (!request || request.type !== 'LANGUAGE_UPDATE_REQUEST') return false;
 return setLocale(request.locale);
}
module.exports = { configure, start, stop, setLocale, onLanguageUpdateRequest };
