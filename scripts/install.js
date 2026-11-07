#!/usr/bin/env node
"use strict";

const childProcess = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");
const zccacheContract = require("./zccache-contract");

const PACKAGE_ROOT = path.resolve(__dirname, "..");
const PACKAGE_JSON = require(path.join(PACKAGE_ROOT, "package.json"));

// Every release ships a single .tar.zst per target that bundles soldr
// alongside soldr-daemon, same-target crgx, and same-target
// cargo-chef. One fetch installs everything, and `bin/soldr.js` wires
// SOLDR_CRGX_LOCAL_DIR and SOLDR_CARGO_CHEF_LOCAL_DIR to the install dir
// so soldr's runtime resolver finds those bundled tools without going
// through the managed-download path.
const ARCHIVE_EXT = zccacheContract.ARCHIVE_EXT;

const TARGETS = {
  "linux-x64-gnu": { triple: "x86_64-unknown-linux-gnu", binary: "soldr" },
  "linux-x64-musl": { triple: "x86_64-unknown-linux-musl", binary: "soldr" },
  "linux-arm64-gnu": { triple: "aarch64-unknown-linux-gnu", binary: "soldr" },
  "linux-arm64-musl": { triple: "aarch64-unknown-linux-musl", binary: "soldr" },
  "darwin-x64": { triple: "x86_64-apple-darwin", binary: "soldr" },
  "darwin-arm64": { triple: "aarch64-apple-darwin", binary: "soldr" },
  "win32-x64": { triple: "x86_64-pc-windows-msvc", binary: "soldr.exe" },
  "win32-arm64": { triple: "aarch64-pc-windows-msvc", binary: "soldr.exe" },
};

// Files we expect to find at the root of every extracted release
// archive. Names line up with what `release-auto.yml`'s
// `Stage soldr release binaries`, `Build crgx from pinned source`, and
// `Build cargo-chef from pinned source` steps drop into `dist/package/`
// before the tar.zst is built. `.exe` suffix is appended at install
// time based on `target.binary`.
const BUNDLED_BINARIES = zccacheContract.RELEASE_BUNDLED_BINARIES;

// The lowest glibc a host must have before the `-gnu` artifact is worth
// downloading (soldr#1060).
//
// This tracks release-auto.yml's BUNDLED-ARCHIVE glibc ceiling
// (`verify_release_bundle.py --check glibc-baseline`), because the installer
// downloads the whole archive. soldr's own binaries reach glibc 2.17 through
// the managed sysroot (soldr#1060, verified in manylinux2014), but the
// archive also ships prebuilt `crgx` and `cargo-chef` from soldr-toolchain,
// which measure GLIBC_2.39. Until those are rebuilt against 2.17, a host
// below 2.39 must get the musl archive, whose tools all run.
//
// Kept in lockstep with that ceiling by a check in test-npm-package.js.
const MIN_GLIBC_FOR_GNU = "2.39";

function compareVersions(left, right) {
  // Numeric, part by part. A lexical compare would rank "2.9" above "2.39"
  // and wave through a host that cannot run the binary.
  const a = String(left).split(".").map((p) => parseInt(p, 10) || 0);
  const b = String(right).split(".").map((p) => parseInt(p, 10) || 0);
  for (let i = 0; i < Math.max(a.length, b.length); i += 1) {
    const diff = (a[i] || 0) - (b[i] || 0);
    if (diff !== 0) {
      return diff < 0 ? -1 : 1;
    }
  }
  return 0;
}

// Decide which Linux artifact this host should download. Ordered probes:
//
//   1. A musl loader in /lib means the SYSTEM is musl, and that outranks
//      whatever Node was linked against. This has to run first: a glibc Node
//      on alpine (`apk add nodejs-current`) reports a perfectly good glibc
//      version, so checking Node's header first would answer "gnu" and never
//      consult the filesystem at all — which is precisely the case the probe
//      was written for.
//   2. Node's reported runtime glibc. This is the only source that gives a
//      VERSION, and gnu is chosen only at or above MIN_GLIBC_FOR_GNU.
//   3. Anything else → musl.
//
// musl is the safe end of every unknown because that artifact is verified
// statically linked before it is ever staged (release-auto.yml → "Verify musl
// binary is statically linked"), so it has no dynamic loader dependency and
// runs on glibc hosts too. The mistakes are not symmetric:
//
//   pick musl, actually glibc      → works, nothing to resolve
//   pick gnu,  actually musl       → hard failure, "soldr: not found"
//   pick gnu,  glibc too old       → hard failure, "GLIBC_2.39 not found"
//
// Only the first is recoverable.
//
// `probes` exists so the branches can be tested on any host; the defaults are
// the real detectors.
function detectLibc(platform = process.platform, probes = {}) {
  if (platform !== "linux") {
    return null;
  }
  const readHeader =
    probes.readHeader ||
    (() => process.report && process.report.getReport && process.report.getReport().header);
  const listLib = probes.listLib || (() => fs.readdirSync("/lib"));

  try {
    const entries = listLib();
    if (entries.some((name) => /^ld-musl-.+\.so\.1$/.test(name))) {
      return "musl";
    }
  } catch (err) {
    // /lib may not be readable in heavily sandboxed containers; fall through.
  }
  try {
    const header = readHeader();
    const runtime = header && header.glibcVersionRuntime;
    if (
      typeof runtime === "string" &&
      runtime.length > 0 &&
      compareVersions(runtime, MIN_GLIBC_FOR_GNU) >= 0
    ) {
      return "gnu";
    }
  } catch (err) {
    // process.report can throw on locked-down environments; fall through.
  }
  return "musl";
}

function platformTarget(platform = process.platform, arch = process.arch, libc = detectLibc(platform)) {
  const key =
    platform === "linux"
      ? `${platform}-${arch}-${libc || "musl"}`
      : `${platform}-${arch}`;
  const target = TARGETS[key];
  if (!target) {
    throw new Error(`unsupported platform for soldr npm package: ${key}`);
  }
  return target;
}

// The ORDERED list of release artifacts this host may install (soldr#1060).
//
// The RFC's policy is "failure -> try the corresponding sibling build", and
// the honest generalization is "retry the OTHER libc", not "always retry gnu":
// `detectLibc` already prefers gnu when the host's glibc is at or above
// MIN_GLIBC_FOR_GNU, so on those hosts the fallback is musl and on every other
// Linux host it is gnu. Both directions are real: a gnu-preferring host whose
// -gnu artifact is missing from a release still has a musl artifact that runs
// there, and a musl-preferring host whose musl artifact 404s can still try gnu.
//
// Non-Linux platforms are single-candidate: there is no sibling libc to fall
// back to, and silently installing a different macOS/Windows triple would be
// wrong rather than merely slower.
function platformCandidates(platform = process.platform, arch = process.arch, libc = detectLibc(platform)) {
  // Resolve the preferred artifact through `platformTarget` rather than
  // rebuilding the key rule here. Two copies of "which artifact does this host
  // want?" are free to disagree silently: a hand-rolled `libc === "gnu" ? ... :
  // "musl"` would quietly answer musl for an unrecognised libc string that
  // `platformTarget` treats as a hard error, so the two entry points would
  // return different things for the same input. One implementation, one answer
  // -- including the unsupported-platform throw.
  const primary = platformTarget(platform, arch, libc);
  if (platform !== "linux") {
    return [primary];
  }
  // Read the preferred libc back off the RESOLVED triple instead of off the
  // `libc` argument: platformTarget has already applied its own
  // `libc || "musl"` default, and re-deriving it is exactly where a second
  // copy would drift from the first.
  const sibling = primary.triple.endsWith("-gnu") ? "musl" : "gnu";
  const candidates = [primary];
  try {
    const fallback = platformTarget(platform, arch, sibling);
    if (fallback.triple !== primary.triple) {
      candidates.push(fallback);
    }
  } catch (err) {
    // A future arch published for only one libc (the RFC's deferred armv7 is
    // musl-only) has no sibling. One candidate is correct there, not an error.
  }
  return candidates;
}

function releaseBaseUrl(version) {
  const override = process.env.SOLDR_NPM_RELEASE_BASE_URL;
  if (override) {
    return override.replace(/\/+$/, "");
  }
  return `https://github.com/zackees/soldr/releases/download/v${version}`;
}

function download(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    const client = url.startsWith("https:") ? https : http;
    const request = client.get(
      url,
      {
        headers: {
          "User-Agent": `soldr-npm/${PACKAGE_JSON.version}`,
        },
      },
      (response) => {
        if (
          response.statusCode >= 300 &&
          response.statusCode < 400 &&
          response.headers.location
        ) {
          response.resume();
          if (redirects >= 5) {
            reject(new Error(`too many redirects while downloading ${url}`));
            return;
          }
          resolve(download(new URL(response.headers.location, url).toString(), redirects + 1));
          return;
        }

        if (response.statusCode !== 200) {
          response.resume();
          reject(new Error(`download failed for ${url}: HTTP ${response.statusCode}`));
          return;
        }

        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => resolve(Buffer.concat(chunks)));
      },
    );
    request.on("error", reject);
  });
}

function checksumFor(checksumsText, filename) {
  for (const line of checksumsText.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    const [hash, name] = trimmed.split(/\s+/, 2);
    if (name === filename) {
      return hash.toLowerCase();
    }
  }
  throw new Error(`checksum entry not found for ${filename}`);
}

// The integrity check for everything this package installs
// (docs/TRUST_BOUNDARIES.md). Extracted from `install()` so it can be
// tested: while it was inline, `checksumFor` was covered but the comparison
// itself was not, so deleting the mismatch branch would have disabled
// verification with every test still green.
//
// Throws rather than returning a boolean, because the only correct response
// to a mismatch is to stop, and a caller that forgot to check a returned
// false would install the archive anyway.
function verifyArchiveChecksum(archive, checksumsText, filename) {
  const expected = checksumFor(checksumsText, filename);
  const actual = crypto.createHash("sha256").update(archive).digest("hex");
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${filename}: expected ${expected}, got ${actual}`);
  }
  return actual;
}

function run(command, args, options = {}) {
  const result = childProcess.spawnSync(command, args, {
    stdio: "inherit",
    ...options,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with exit code ${result.status}`);
  }
}

function extractArchive(archivePath, destination) {
  // GNU tar 1.31+ and bsdtar (default on macOS / Windows) both support
  // `--zstd` for zstandard. If a host's tar predates that flag, fall
  // back to `--use-compress-program=unzstd` which only needs the
  // `unzstd` CLI on PATH — installed alongside `zstd` on every modern
  // package manager. As a last resort, `zstd` decompresses to a temp
  // .tar and we extract that explicitly.
  const attempts = [
    ["tar", ["--zstd", "-xf", archivePath, "-C", destination]],
    ["tar", ["--use-compress-program=unzstd", "-xf", archivePath, "-C", destination]],
  ];
  for (const [cmd, args] of attempts) {
    const result = childProcess.spawnSync(cmd, args, { stdio: "inherit" });
    if (!result.error && result.status === 0) {
      return;
    }
    if (result.error && result.error.code === "ENOENT") {
      throw result.error;
    }
    // status != 0 → fall through to the next strategy.
  }
  // Last resort: decompress to a sibling .tar, then untar.
  const intermediate = `${archivePath}.tar`;
  run("zstd", ["-d", "-o", intermediate, archivePath]);
  run("tar", ["-xf", intermediate, "-C", destination]);
  fs.rmSync(intermediate, { force: true });
}

function findExtractedBinary(root, binaryName) {
  const entries = fs.readdirSync(root, { withFileTypes: true });
  for (const entry of entries) {
    const candidate = path.join(root, entry.name);
    if (entry.isFile() && entry.name === binaryName) {
      return candidate;
    }
    if (entry.isDirectory()) {
      const nested = findExtractedBinary(candidate, binaryName);
      if (nested) {
        return nested;
      }
    }
  }
  return null;
}

// Download, verify, and install one candidate target's release archive into
// bin/native/. Extracted from install() (soldr#1060) so a retry loop can call
// it once per candidate.
//
// A failed candidate must not leave bin/native/ half-populated for the next
// one, so every source path is resolved and validated first: manifest
// location + parse + validateReleaseManifest, then every findExtractedBinary
// lookup for BUNDLED_BINARIES and soldrDebugInfoEntries. Only once every
// source is known-present does bin/native/ get cleared and repopulated.
async function installRelease(target, version, baseUrl) {
  const filename = `soldr-v${version}-${target.triple}.${ARCHIVE_EXT}`;
  const archiveUrl = `${baseUrl}/${filename}`;
  const checksumUrl = `${baseUrl}/soldr-v${version}-SHA256SUMS.txt`;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "soldr-npm-"));

  try {
    console.log(`soldr: downloading ${archiveUrl}`);
    const [archive, checksums] = await Promise.all([
      download(archiveUrl),
      download(checksumUrl).then((buffer) => buffer.toString("utf8")),
    ]);

    verifyArchiveChecksum(archive, checksums, filename);

    const archivePath = path.join(tmp, filename);
    const extractDir = path.join(tmp, "extract");
    fs.writeFileSync(archivePath, archive);
    fs.mkdirSync(extractDir, { recursive: true });
    extractArchive(archivePath, extractDir);

    const nativeDir = path.join(PACKAGE_ROOT, "bin", "native");

    // Resolve every source before touching bin/native/. `bin/soldr.js`
    // wires SOLDR_CRGX_LOCAL_DIR / SOLDR_CARGO_CHEF_LOCAL_DIR to this dir,
    // so soldr has its daemon and can find crgx / cargo-chef once this
    // completes. The archive layout is flat: all bundled binaries live at
    // the archive root.
    const binaryExt = target.binary.endsWith(".exe") ? ".exe" : "";
    const manifestSrc = findExtractedBinary(extractDir, zccacheContract.MANIFEST_NAME);
    if (!manifestSrc) {
      throw new Error(`release archive ${filename} did not contain ${zccacheContract.MANIFEST_NAME}`);
    }
    const manifest = JSON.parse(fs.readFileSync(manifestSrc, "utf8"));
    zccacheContract.validateReleaseManifest(manifest, {
      soldrTarget: target.triple,
      platform: process.platform,
      findFile: (name) => {
        const filePath = findExtractedBinary(extractDir, name);
        if (!filePath) {
          throw new Error(`release archive ${filename} did not contain ${name}`);
        }
        return filePath;
      },
    });

    const copyPlan = [];
    for (const baseName of BUNDLED_BINARIES) {
      const fileName = `${baseName}${binaryExt}`;
      const src = findExtractedBinary(extractDir, fileName);
      if (!src) {
        throw new Error(`release archive ${filename} did not contain ${fileName}`);
      }
      copyPlan.push({ src, dst: path.join(nativeDir, fileName), executable: true });
    }
    for (const entry of zccacheContract.soldrDebugInfoEntries(manifest)) {
      const src = findExtractedBinary(extractDir, entry.name);
      if (!src) {
        throw new Error(`release archive ${filename} did not contain ${entry.name}`);
      }
      copyPlan.push({ src, dst: path.join(nativeDir, entry.name), executable: false });
    }
    // Drop manifest.json alongside the binaries so downstream tooling
    // (and humans reading `bin/native/`) can introspect provenance —
    // soldr / zccache versions, target triples, build commit, sha256s.
    copyPlan.push({
      src: manifestSrc,
      dst: path.join(nativeDir, zccacheContract.MANIFEST_NAME),
      executable: false,
    });

    // Every source is known-present now — safe to clear and repopulate.
    fs.rmSync(nativeDir, { recursive: true, force: true });
    fs.mkdirSync(nativeDir, { recursive: true });
    for (const { src, dst, executable } of copyPlan) {
      fs.copyFileSync(src, dst);
      if (executable && process.platform !== "win32") {
        fs.chmodSync(dst, 0o755);
      }
    }

    console.log(
      `soldr: installed ${target.triple} (soldr + daemon + crgx + cargo-chef) into ${nativeDir}`,
    );
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// Walk `candidates` in order, returning the first that installs. `attempt` is
// injected (rather than inlined into install()) so the fallthrough policy can
// be tested without a network: an untestable retry loop is how "we have a
// fallback" becomes true in the comments and false in the code.
async function installFirstWorkingCandidate(candidates, attempt, log = console) {
  if (!Array.isArray(candidates) || candidates.length === 0) {
    // A zero-length list would fall straight through the loop to a rejection
    // whose detail string is empty -- "could not install ()" tells nobody
    // anything. platformCandidates never produces one (it throws instead), so
    // reaching here means the caller built the list some other way.
    throw new Error("no soldr release candidates to install (empty candidate list)");
  }
  const failures = [];
  for (let index = 0; index < candidates.length; index += 1) {
    const target = candidates[index];
    try {
      await attempt(target);
      if (index > 0) {
        log.warn(
          `soldr: installed the sibling libc build ${target.triple} instead of the ` +
            `preferred ${failures[0].target.triple} (${failures.length} candidate(s) failed first)`,
        );
      }
      return target;
    } catch (error) {
      failures.push({ target, error });
      const next = candidates[index + 1];
      if (next) {
        log.warn(
          `soldr: ${target.triple} failed (${error.message}); falling back to the sibling libc build ${next.triple}`,
        );
      }
    }
  }
  const detail = failures
    .map(({ target, error }) => `${target.triple}: ${error.message}`)
    .join("; ");
  throw new Error(`no soldr release artifact could be installed (${detail})`);
}

async function install() {
  if (process.env.SOLDR_NPM_SKIP_DOWNLOAD) {
    console.log("soldr: skipping native binary download because SOLDR_NPM_SKIP_DOWNLOAD is set");
    return;
  }
  const version = PACKAGE_JSON.version;
  const baseUrl = releaseBaseUrl(version);
  const candidates = platformCandidates();
  await installFirstWorkingCandidate(candidates, (target) =>
    installRelease(target, version, baseUrl),
  );
}

if (require.main === module) {
  install().catch((error) => {
    console.error(`soldr: npm install failed: ${error.message}`);
    process.exit(1);
  });
}

module.exports = {
  ARCHIVE_EXT,
  MIN_GLIBC_FOR_GNU,
  compareVersions,
  BUNDLED_BINARIES,
  TARGETS,
  checksumFor,
  verifyArchiveChecksum,
  detectLibc,
  platformTarget,
  platformCandidates,
  installFirstWorkingCandidate,
  releaseBaseUrl,
};
