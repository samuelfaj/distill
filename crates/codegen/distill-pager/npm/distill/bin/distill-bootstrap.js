#!/usr/bin/env node
// Modified for Distill by Samuel Fajreldines, 2026.
// Resolves the distill binary and runs it, in order of preference:
//   1. $DISTILL_HOME/bin/distill, the versioned symlink postinstall.js installs
//   2. bootstrap it from the per-platform @distill/cli-<platform>
//      package, decompressing the compressed binary into $DISTILL_HOME/bin
//   3. decompress in place under node_modules (no resolvable version, or
//      an unwritable home)
//
// Binaries ship brotli-compressed to stay under npm's tarball size limit.
const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');
const os = require('os');
const zlib = require('zlib');

const pkgName = '@distill/cli';
const IS_WINDOWS = process.platform === 'win32';
const EXE = IS_WINDOWS ? '.exe' : '';
const BIN_NAME = `distill${EXE}`;
// $DISTILL_HOME/bin (else ~/.distill/bin), matching the Rust distill_home():
// a symlinked $HOME resolves the same way.
function defaultDistillHome() {
    const home = os.homedir();
    try { return path.join(fs.realpathSync(home), '.distill'); } catch { return path.join(home, '.distill'); }
}
const DISTILL_HOME = process.env.DISTILL_HOME ?? defaultDistillHome();
const CANONICAL_DIR = path.join(DISTILL_HOME, 'bin');
const CANONICAL_PATH = path.join(CANONICAL_DIR, BIN_NAME);

function readLocalVersion() {
    try { return require('../package.json').version; } catch { return undefined; }
}

// Returns null when npm skipped the matching optional dependency
// (unsupported platform, or --no-optional).
function resolvePlatformPackageDir() {
    const platformPkg = `@distill/cli-${process.platform}-${process.arch}`;
    try {
        return path.dirname(require.resolve(`${platformPkg}/package.json`));
    } catch {
        return null;
    }
}

function writeVendorBinary(brotliPath, binaryPath, destPath) {
    const tmp = destPath + `.tmp.${process.pid}`;
    try {
        if (fs.existsSync(brotliPath)) {
            fs.writeFileSync(tmp, zlib.brotliDecompressSync(fs.readFileSync(brotliPath)));
        } else if (fs.existsSync(binaryPath)) {
            fs.copyFileSync(binaryPath, tmp);
        } else {
            return false;
        }
        if (!IS_WINDOWS) fs.chmodSync(tmp, 0o755);
        fs.renameSync(tmp, destPath);
        return true;
    } catch {
        return false;
    } finally {
        try { fs.unlinkSync(tmp); } catch {}
    }
}

function swapCanonical(versionedName, versionedPath) {
    if (!IS_WINDOWS) {
        const tmpLink = CANONICAL_PATH + `.link.${process.pid}`;
        try { fs.unlinkSync(tmpLink); } catch {}
        fs.symlinkSync(versionedName, tmpLink);
        fs.renameSync(tmpLink, CANONICAL_PATH);
        return;
    }
    const oldPath = CANONICAL_PATH + '.old';
    try { fs.unlinkSync(oldPath); } catch {}
    try {
        try { fs.unlinkSync(CANONICAL_PATH); } catch {}
        fs.copyFileSync(versionedPath, CANONICAL_PATH);
    } catch {
        fs.renameSync(CANONICAL_PATH, oldPath);
        try {
            fs.copyFileSync(versionedPath, CANONICAL_PATH);
        } catch {
            try { fs.renameSync(oldPath, CANONICAL_PATH); } catch {}
            throw new Error('locked');
        }
    }
}

function bootstrapCanonical(brotliPath, binaryPath, version) {
    try {
        fs.mkdirSync(CANONICAL_DIR, { recursive: true });
        const versionedName = `distill-${version}${EXE}`;
        const versionedPath = path.join(CANONICAL_DIR, versionedName);
        if (!fs.existsSync(versionedPath) && !writeVendorBinary(brotliPath, binaryPath, versionedPath)) {
            return null;
        }
        swapCanonical(versionedName, versionedPath);
        // null on a broken wire-up so the caller falls back to in-place launch.
        return fs.existsSync(CANONICAL_PATH) ? CANONICAL_PATH : null;
    } catch {
        return null;
    }
}

function resolveBinary() {
    if (fs.existsSync(CANONICAL_PATH)) return CANONICAL_PATH;

    const platformDir = resolvePlatformPackageDir();
    if (!platformDir) {
        console.error(`${pkgName}: no platform binary installed for ${process.platform}-${process.arch}.`);
        console.error(`  Expected sibling package @distill/cli-${process.platform}-${process.arch}.`);
        console.error(`  This usually means npm skipped optionalDependencies (e.g. --no-optional)`);
        console.error(`  or the platform is not supported.`);
        process.exit(1);
    }

    const binaryPath = path.join(platformDir, 'bin', BIN_NAME);
    const brotliPath = binaryPath + '.br';
    const version = readLocalVersion();

    if (version) {
        const bootstrapped = bootstrapCanonical(brotliPath, binaryPath, version);
        if (bootstrapped) return bootstrapped;
    }

    if (!fs.existsSync(binaryPath) && !writeVendorBinary(brotliPath, binaryPath, binaryPath)) {
        console.error(`${pkgName}: missing binary at ${binaryPath}`);
        process.exit(1);
    }
    return binaryPath;
}

const execPath = resolveBinary();
const childEnv = { ...process.env, DISTILL_MANAGED_BY_NPM: '1' };
const child = spawn(execPath, process.argv.slice(2), { stdio: 'inherit', env: childEnv });
child.on('exit', (code, signal) => {
    if (signal) {
        process.kill(process.pid, signal);
    } else {
        process.exit(code ?? 0);
    }
});
