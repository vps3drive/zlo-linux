'use strict';

const { describe, it } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('fs');
const path = require('path');
const { getAvailableTargets } = require('../scripts/build-native');
const { getOnnxRuntimeUrl, getOnnxRuntimeEntry, verifyOnnxRuntimeArchive, ONNXRUNTIME_VERSION, ONNXRUNTIME_SHA512 } = require('../scripts/download');
const { ROOT } = require('../scripts/utils/paths');

describe('Native Build Orchestrator & Downloads', () => {
  it('discovers all 10 native addons and 2 native engines (pure Rust)', () => {
    const targets = getAvailableTargets();
    const names = targets.map(t => t.name);

    const expectedAddons = [
      'db-cross-v4',
      'file-utilities',
      'file-utils',
      'mp4thumb',
      'os-sys-info',
      'zfile',
      'zimage',
      'zjxl',
      'zlang',
      'zwalker',
    ];

    for (const addon of expectedAddons) {
      assert.ok(names.includes(addon), `Expected addon ${addon} to be discovered in native targets`);
      const t = targets.find(item => item.name === addon);
      assert.strictEqual(t.category, 'addon');
      assert.ok(fs.existsSync(path.join(t.dir, 'Cargo.toml')), `Addon ${addon} must have Cargo.toml (pure Rust)`);
    }

    assert.ok(names.includes('zocr'), 'Expected engine zocr to be discovered');
    assert.ok(names.includes('zcall-audio'), 'Expected engine zcall-audio to be discovered');
    assert.ok(names.includes('zportal'), 'Expected engine zportal to be discovered');
  });

  it('points ONNX Runtime at the pinned NuGet package and the right library', () => {
    assert.ok(ONNXRUNTIME_VERSION, 'ONNXRUNTIME_VERSION must be defined');
    assert.strictEqual(getOnnxRuntimeUrl(),
      `https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ONNXRUNTIME_VERSION}/microsoft.ml.onnxruntime.${ONNXRUNTIME_VERSION}.nupkg`);
    assert.strictEqual(getOnnxRuntimeEntry('x64'), 'runtimes/linux-x64/native/libonnxruntime.so');
    assert.strictEqual(getOnnxRuntimeEntry('arm64'), 'runtimes/linux-arm64/native/libonnxruntime.so');
    assert.strictEqual(Buffer.from(ONNXRUNTIME_SHA512, 'base64').length, 64, 'SHA-512 pin must be 64 bytes');
  });

  it('rejects an ONNX Runtime package that is not the pinned one', () => {
    const tmp = path.join(require('os').tmpdir(), `ort-test-${process.pid}.nupkg`);
    fs.writeFileSync(tmp, 'not the package');
    try {
      assert.throws(() => verifyOnnxRuntimeArchive(tmp), /SHA-512/);
    } finally {
      fs.rmSync(tmp, { force: true });
    }
  });
});
