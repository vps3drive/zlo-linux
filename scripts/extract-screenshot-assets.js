/**
 * extract-screenshot-assets.js
 *
 * Extracts Zalo's screen capture icon font from app/pc-dist/fonts/
 * into app/screenshot-assets/ui/fonts/zalo-pc-screencap.ttf at build/prepare time.
 * Nothing is committed to source control: the proprietary font is extracted
 * from the app bundle.
 *
 * Also installs the portal client (native/engines/zportal, built by
 * build-native) as app/screenshot-assets/zportal: outside app.asar, so the
 * screenshot plugin can run it.
 */

const fs = require('fs-extra');
const path = require('path');
const logger = require('./utils/logger');

const { APP_DIR, ROOT, resolvePrebuiltBinary } = require('./utils/paths');
const OUT_DIR = path.join(APP_DIR, 'screenshot-assets', 'ui', 'fonts');

function findFont(dir) {
  if (!fs.existsSync(dir)) return null;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      const res = findFont(fullPath);
      if (res) return res;
    } else if (/^zalo-pc-screencap(\.[0-9a-f]+)?\.ttf$/.test(entry.name)) {
      return fullPath;
    }
  }
  return null;
}

async function main(options = {}) {
  const appDir = (options && options.appDir) || APP_DIR;
  const outDir = (options && options.outDir) || OUT_DIR;

  const pcFontsDir = path.join(appDir, 'pc-dist', 'fonts');
  const fontFile = findFont(pcFontsDir) || findFont(appDir);

  if (!fontFile) {
    throw new Error('Screenshot font zalo-pc-screencap.*.ttf not found in app directory');
  }

  fs.ensureDirSync(outDir);
  const targetFile = path.join(outDir, 'zalo-pc-screencap.ttf');
  fs.copySync(fontFile, targetFile);

  logger.success(`Screenshot font extracted: ${path.basename(fontFile)} -> app/screenshot-assets/ui/fonts/zalo-pc-screencap.ttf`);

  deployPortal(appDir);
}

/** Copy the zportal binary to <appDir>/screenshot-assets/zportal. False if it is not built. */
function deployPortal(appDir = APP_DIR) {
  const source = resolvePrebuiltBinary('zportal', path.join(ROOT, 'target', 'release', 'zportal'));
  if (!fs.existsSync(source)) {
    logger.warn('zportal not built: screenshots skip the desktop portal (run npm run build:native)');
    return false;
  }
  const dest = path.join(appDir, 'screenshot-assets', 'zportal');
  fs.ensureDirSync(path.dirname(dest));
  fs.copySync(source, dest);
  fs.chmodSync(dest, 0o755);
  logger.success('Screenshot portal client installed: app/screenshot-assets/zportal');
  return true;
}

module.exports = { main, findFont, deployPortal };

if (require.main === module) {
  main().catch((e) => {
    logger.error(e.message);
    process.exit(1);
  });
}
