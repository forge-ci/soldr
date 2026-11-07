#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const assert = require("assert");

const root = path.resolve(__dirname, "..");
const pkg = require(path.join(root, "package.json"));
const install = require(path.join(root, "scripts", "install.js"));
const zccacheContract = require(path.join(root, "scripts", "zccache-contract.js"));

function tomlSection(toml, sectionName) {
  const header = `[${sectionName}]`;
  const lines = toml.split(/\r?\n/);
  const body = [];
  let found = false;

  for (const line of lines) {
    if (/^\s*\[.*\]\s*$/.test(line)) {
      if (found) {
        break;
      }
      found = line.trim() === header;
      continue;
    }

    if (found) {
      body.push(line);
    }
  }

  return found ? body.join("\n") : null;
}

const cargoToml = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
const cargoVersion = cargoToml.match(/\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m);
assert(cargoVersion, "workspace package version not found in Cargo.toml");
assert.strictEqual(pkg.version, cargoVersion[1], "package.json version must match Cargo.toml");

const pyprojectToml = fs.readFileSync(path.join(root, "pyproject.toml"), "utf8");
const pyprojectProject = tomlSection(pyprojectToml, "project");
assert(pyprojectProject, "[project] section not found in pyproject.toml");

assert(
  !/^\s*version\s*=/.test(pyprojectProject),
  'pyproject.toml [project] must not hardcode version; PyPI must derive it from Cargo.toml',
);

const dynamicVersion = pyprojectProject.match(/^\s*dynamic\s*=\s*\[([^\]]*)\]\s*$/m);
assert(
  dynamicVersion,
  'pyproject.toml [project] must declare dynamic = ["version"] so PyPI derives from Cargo.toml',
);

const dynamicItems = [...dynamicVersion[1].matchAll(/"([^"]+)"|'([^']+)'/g)].map(
  (match) => match[1] || match[2],
);
assert(
  dynamicItems.includes("version"),
  'pyproject.toml [project] dynamic metadata must include "version"',
);

assert.strictEqual(pkg.name, "@zackees/soldr");
assert.strictEqual(pkg.license, "BSD-3-Clause");
assert.strictEqual(pkg.bin.soldr, "bin/soldr.js");
assert.strictEqual(pkg.repository.url, "git+https://github.com/zackees/soldr.git");
assert.deepStrictEqual(pkg.files, [
  "bin/soldr.js",
  "contracts/zccache-integration-guardrails.v1.json",
  "contracts/zccache-runtime.v1.json",
  "scripts/install.js",
  "scripts/zccache-contract.js",
  "scripts/test-npm-package.js",
  "README.md",
  "LICENSE",
]);

const bin = fs.readFileSync(path.join(root, pkg.bin.soldr), "utf8");
assert(bin.startsWith("#!/usr/bin/env node"), "bin/soldr.js must have a node shebang");

// Linux: triple selection branches on libc. `platformTarget` accepts an
// explicit `libc` arg so tests don't depend on the runtime detector.
assert.strictEqual(
  install.platformTarget("linux", "x64", "gnu").triple,
  "x86_64-unknown-linux-gnu",
);
assert.strictEqual(
  install.platformTarget("linux", "x64", "musl").triple,
  "x86_64-unknown-linux-musl",
);
assert.strictEqual(
  install.platformTarget("linux", "arm64", "gnu").triple,
  "aarch64-unknown-linux-gnu",
);
assert.strictEqual(
  install.platformTarget("linux", "arm64", "musl").triple,
  "aarch64-unknown-linux-musl",
);
// Default (no libc arg) falls back to detectLibc; on most CI hosts that's
// gnu. We don't assert the triple here — just that the call resolves.
assert.ok(install.platformTarget("linux", "x64").triple.startsWith("x86_64-unknown-linux-"));
// An explicitly-unknown libc must take the runs-anywhere build, matching
// detectLibc's own unknown case rather than contradicting it.
assert.strictEqual(
  install.platformTarget("linux", "x64", null).triple,
  "x86_64-unknown-linux-musl",
);
assert.strictEqual(install.platformTarget("darwin", "x64").triple, "x86_64-apple-darwin");
assert.strictEqual(install.platformTarget("darwin", "arm64").triple, "aarch64-apple-darwin");
assert.strictEqual(install.platformTarget("win32", "x64").triple, "x86_64-pc-windows-msvc");
assert.strictEqual(install.platformTarget("win32", "arm64").triple, "aarch64-pc-windows-msvc");
assert.throws(() => install.platformTarget("freebsd", "x64"), /unsupported platform/);

// platformCandidates orders the download attempts for a host. Linux gets a
// preferred triple plus a sibling-libc fallback so a wrong libc guess (or a
// missing release asset) can be retried without a second npm install; every
// other platform has no sibling to fall back to.
const triples = (list) => list.map((t) => t.triple);

// a. A glibc new enough for the shipped artifact prefers gnu, with musl as
//    the fallback if the gnu asset turns out to be missing or broken.
assert.deepStrictEqual(triples(install.platformCandidates("linux", "x64", "gnu")), [
  "x86_64-unknown-linux-gnu",
  "x86_64-unknown-linux-musl",
]);
assert.deepStrictEqual(triples(install.platformCandidates("linux", "arm64", "gnu")), [
  "aarch64-unknown-linux-gnu",
  "aarch64-unknown-linux-musl",
]);

// b. An old/unknown glibc, or a musl system, prefers musl -- gnu would not
//    even run -- but still lists gnu second in case musl's own asset is the
//    one that is missing.
assert.deepStrictEqual(triples(install.platformCandidates("linux", "x64", "musl")), [
  "x86_64-unknown-linux-musl",
  "x86_64-unknown-linux-gnu",
]);
assert.deepStrictEqual(triples(install.platformCandidates("linux", "arm64", "musl")), [
  "aarch64-unknown-linux-musl",
  "aarch64-unknown-linux-gnu",
]);

// c. `libc === null` (both detectLibc probes unavailable) must behave like
//    musl, matching detectLibc's own unknown case (line 105-108) rather than
//    contradicting it -- picking gnu here would be the opposite of the
//    "unknown host, run anywhere" reasoning detectLibc already applies.
assert.deepStrictEqual(triples(install.platformCandidates("linux", "x64", null)), [
  "x86_64-unknown-linux-musl",
  "x86_64-unknown-linux-gnu",
]);

// d. Non-Linux platforms have exactly one candidate: there is no sibling
//    libc, and "falling back" to a different macOS/Windows triple would
//    install a binary that cannot run on the host at all.
assert.deepStrictEqual(triples(install.platformCandidates("darwin", "arm64")), [
  "aarch64-apple-darwin",
]);
assert.strictEqual(install.platformCandidates("darwin", "x64").length, 1);
assert.strictEqual(install.platformCandidates("win32", "x64").length, 1);
assert.strictEqual(install.platformCandidates("win32", "arm64").length, 1);

// e. An arch/platform we do not ship at all is still a hard error, not an
//    empty candidate list a caller could silently iterate zero times over.
assert.throws(() => install.platformCandidates("freebsd", "x64"), /unsupported platform/);
assert.throws(() => install.platformCandidates("linux", "riscv64", "gnu"), /unsupported platform/);

// e2. platformCandidates must resolve its preferred artifact THROUGH
//     platformTarget rather than rebuilding the key rule, so the two can
//     never answer differently for the same input. An unrecognised libc
//     string is where a second hand-rolled `libc === "gnu" ? ... : "musl"`
//     copy would silently answer musl while platformTarget throws.
assert.throws(() => install.platformTarget("linux", "x64", "uclibc"), /unsupported platform/);
assert.throws(() => install.platformCandidates("linux", "x64", "uclibc"), /unsupported platform/);

// f. Structural invariants for every Linux candidate list: the two
//    candidates are distinct, the first one is unchanged from what
//    `platformTarget` already picks for the same args (this feature must
//    not change the shipped preference), and every candidate is a real
//    entry from TARGETS rather than a hand-built object that happens to
//    look right.
for (const [arch, libc] of [
  ["x64", "gnu"],
  ["x64", "musl"],
  ["x64", null],
  ["arm64", "gnu"],
  ["arm64", "musl"],
]) {
  const candidates = install.platformCandidates("linux", arch, libc);
  assert.notStrictEqual(candidates[0].triple, candidates[1].triple);
  assert.deepStrictEqual(candidates[0], install.platformTarget("linux", arch, libc));
  for (const candidate of candidates) {
    assert.ok(
      Object.values(install.TARGETS).includes(candidate),
      `platformCandidates("linux", "${arch}", ${libc}) returned an object not in TARGETS`,
    );
  }
}

// detectLibc must return null on non-Linux platforms so the platform key
// stays `<platform>-<arch>` rather than `<platform>-<arch>-gnu`.
assert.strictEqual(install.detectLibc("darwin"), null);
assert.strictEqual(install.detectLibc("win32"), null);
// On Linux it must always resolve to one of the two known families so
// `platformTarget` can build a well-formed key.
const linuxLibc = install.detectLibc("linux");
assert.ok(linuxLibc === "gnu" || linuxLibc === "musl", `unexpected libc: ${linuxLibc}`);

// detectLibc's branches, driven through the probe seam so they are
// exercised on every host rather than only on whichever libc CI runs.
const throwingProbe = () => {
  throw new Error("probe unavailable");
};
const NEW_ENOUGH = install.MIN_GLIBC_FOR_GNU;

// Numeric version comparison. A lexical compare ranks "2.9" above "2.39"
// and would wave through a host that cannot run the binary.
assert.strictEqual(install.compareVersions("2.9", "2.39"), -1);
assert.strictEqual(install.compareVersions("2.39", "2.9"), 1);
assert.strictEqual(install.compareVersions("2.39", "2.39"), 0);
assert.strictEqual(install.compareVersions("2.40", "2.39"), 1);
assert.strictEqual(install.compareVersions("2.2.5", "2.14"), -1);

// 1. A glibc new enough for the shipped artifact takes the gnu build.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({ glibcVersionRuntime: NEW_ENOUGH }),
    listLib: () => ["ld-linux-x86-64.so.2", "libc.so.6"],
  }),
  "gnu",
);

// 2. A glibc that is too old must NOT take the gnu build. This is the live
//    bug soldr#1060 exists to prevent: a host whose glibc is below what the
//    published gnu binary actually requires must not be sent to it, or it
//    dies with "version `GLIBC_2.xx' not found" while the musl artifact
//    from the same release runs fine. RHEL 7 / CentOS 7 (glibc 2.17,
//    exactly the shipped floor) is the boundary case; one below it is
//    unambiguously too old regardless of where the floor currently sits.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({ glibcVersionRuntime: "2.12" }),
    listLib: () => ["ld-linux-x86-64.so.2", "libc.so.6"],
  }),
  "musl",
);

// 3. A musl SYSTEM wins over whatever Node was linked against. Node built
//    against glibc on alpine (`apk add nodejs-current`) reports a perfectly
//    good glibc version, so the filesystem probe has to be consulted FIRST --
//    otherwise this answers "gnu" and never looks at /lib at all.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({ glibcVersionRuntime: NEW_ENOUGH }),
    listLib: () => ["ld-musl-x86_64.so.1"],
  }),
  "musl",
);

// 4. Real musl Node omits the property entirely (verified on node:22-alpine:
//    hasOwnProperty is false, not present-and-empty).
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: () => ({}),
    listLib: () => ["ld-musl-x86_64.so.1"],
  }),
  "musl",
);

// 5. Both probes unavailable (heavily sandboxed container) -> musl, the only
//    artifact that runs without knowing anything about the host.
assert.strictEqual(
  install.detectLibc("linux", { readHeader: throwingProbe, listLib: throwingProbe }),
  "musl",
);

// 6. A glibc host whose version cannot be read is also unknown: /lib says
//    glibc but carries no version, so the floor cannot be confirmed.
assert.strictEqual(
  install.detectLibc("linux", {
    readHeader: throwingProbe,
    listLib: () => ["ld-linux-x86-64.so.2", "libc.so.6"],
  }),
  "musl",
);

// The probe seam must not leak to other platforms: a non-Linux host still
// short-circuits to null before any probe runs.
assert.strictEqual(
  install.detectLibc("darwin", { readHeader: throwingProbe, listLib: throwingProbe }),
  null,
);

// g. Candidate ordering driven end to end from the HOST's glibc rather than
//    from a literal libc string. The RFC states its cases as "new glibc" /
//    "old glibc", and `libc` is only detectLibc's answer to that question --
//    asserting the order for the string "gnu" proves the ordering helper
//    works, not that a new-glibc host reaches it. Composing the two is what
//    covers the path install() actually walks.
{
  // One step below the shipped floor, derived rather than hardcoded. A
  // literal "2.36" here would stop testing the too-old branch the day
  // MIN_GLIBC_FOR_GNU drops to 2.17 -- it would silently become a
  // new-ENOUGH version and this case would assert the opposite of its name.
  const floorParts = install.MIN_GLIBC_FOR_GNU.split(".").map((p) => parseInt(p, 10) || 0);
  floorParts[floorParts.length - 1] -= 1;
  const TOO_OLD = floorParts.join(".");
  assert.strictEqual(
    install.compareVersions(TOO_OLD, install.MIN_GLIBC_FOR_GNU),
    -1,
    `derived too-old glibc ${TOO_OLD} must sort below ${install.MIN_GLIBC_FOR_GNU}`,
  );

  const GLIBC_LIB = ["ld-linux-x86-64.so.2", "libc.so.6"];
  const candidatesForHost = (probes) =>
    triples(install.platformCandidates("linux", "x64", install.detectLibc("linux", probes)));

  // New-enough glibc -> gnu first, musl second as the sibling fallback.
  assert.deepStrictEqual(
    candidatesForHost({
      readHeader: () => ({ glibcVersionRuntime: NEW_ENOUGH }),
      listLib: () => GLIBC_LIB,
    }),
    ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"],
  );
  // Too-old glibc (the Debian 12 case that motivated soldr#1060) -> musl
  // first, because gnu would not even load, but gnu is still listed second
  // so a missing/broken musl asset is recoverable without a second install.
  assert.deepStrictEqual(
    candidatesForHost({
      readHeader: () => ({ glibcVersionRuntime: TOO_OLD }),
      listLib: () => GLIBC_LIB,
    }),
    ["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"],
  );
  // A musl SYSTEM reaches the same order through the /lib probe.
  assert.deepStrictEqual(
    candidatesForHost({ readHeader: () => ({}), listLib: () => ["ld-musl-x86_64.so.1"] }),
    ["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"],
  );
  // Both probes dead -> still musl-first, never an empty or single list that
  // would quietly drop the fallback on exactly the hosts that need it most.
  assert.deepStrictEqual(
    candidatesForHost({ readHeader: throwingProbe, listLib: throwingProbe }),
    ["x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"],
  );
}

// h. The zero-argument shape install() itself calls: on this host the first
//    candidate must still be exactly what platformTarget picks today. The
//    retry may add a fallback; it may not change which artifact is preferred.
{
  const hostCandidates = install.platformCandidates();
  assert.deepStrictEqual(hostCandidates[0], install.platformTarget());
  assert.strictEqual(
    hostCandidates.length,
    process.platform === "linux" ? 2 : 1,
    `platformCandidates() length on ${process.platform}: ${triples(hostCandidates).join(", ")}`,
  );
}

// MIN_GLIBC_FOR_GNU must track the ceiling release-auto.yml enforces on the
// gnu ARCHIVE (see the lockstep assertion below). When that ceiling drops,
// the installer must follow it down -- otherwise glibc hosts keep being sent
// to musl long after gnu would work -- and it must never go below it.
//
// Anchored on the script name rather than on any `--max-glibc`. release-auto
// now passes that flag twice: 2.39 to verify_glibc_baseline.py for the
// binaries, and 2.17 to verify_wheel_glibc.py for the wheel contents. A bare
// match takes whichever appears first, so this was correct only by the order
// the steps happen to sit in. Reorder them and the lockstep would demand the
// installer drop to 2.17, routing glibc 2.17-2.38 hosts to a binary that needs
// 2.39 -- the bug #2081 fixed, reintroduced by its own guard.
function glibcCeilingsFor(workflowText, scriptName) {
  const found = [];
  const lines = workflowText.split(/\r?\n/);
  for (let i = 0; i < lines.length; i += 1) {
    if (!lines[i].includes(scriptName)) {
      continue;
    }
    // The invocation may be split across continuation lines, so look at the
    // matching line and the few that follow it.
    const window = lines.slice(i, i + 4).join("\n");
    const match = window.match(/--max-glibc\s+([0-9][0-9.]*)/);
    if (match) {
      found.push(match[1]);
    }
  }
  return found;
}

function glibcCeilingFor(workflowText, scriptName) {
  const found = glibcCeilingsFor(workflowText, scriptName);
  if (found.length === 0) {
    return null;
  }
  // release-auto invokes verify_glibc_baseline.py twice -- once pre-staging on
  // the built binary, once post-staging across the whole bundle. Returning the
  // first would let the two drift apart with the installer silently following
  // only one of them, which is the same order-dependence this function was
  // written to remove.
  const distinct = [...new Set(found)];
  assert.strictEqual(
    distinct.length,
    1,
    `release-auto.yml passes conflicting --max-glibc values to ${scriptName}: ` +
      `${found.join(", ")}. They gate the same artifacts and must agree.`,
  );
  return distinct[0];
}

// Pin the anchoring itself: with the wheel invocation first, the binary
// ceiling must still resolve to the binary ceiling.
{
  const reordered = [
    "python3 .github/scripts/verify_wheel_glibc.py --max-glibc 2.17 dist/*.whl",
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 soldr",
  ].join("\n");
  assert.strictEqual(
    glibcCeilingFor(reordered, "verify_glibc_baseline.py"),
    "2.39",
    "the binary ceiling must not be confused with the wheel ceiling",
  );
  assert.strictEqual(
    glibcCeilingFor(reordered, "verify_wheel_glibc.py"),
    "2.17",
    "the wheel ceiling must resolve independently",
  );
  assert.strictEqual(glibcCeilingFor(reordered, "not_a_script.py"), null);

  // Two invocations of the same script that agree resolve to that value...
  const agreeing = [
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 soldr",
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 bundle",
  ].join("\n");
  assert.strictEqual(glibcCeilingFor(agreeing, "verify_glibc_baseline.py"), "2.39");

  // ...and two that disagree are a hard error rather than a silent pick.
  const conflicting = [
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.17 soldr",
    "python3 .github/scripts/verify_glibc_baseline.py --max-glibc 2.39 bundle",
  ].join("\n");
  assert.throws(
    () => glibcCeilingFor(conflicting, "verify_glibc_baseline.py"),
    /conflicting --max-glibc/,
    "two ceilings for the same script must not be silently reconciled",
  );
}

const releaseWorkflow = fs.readFileSync(
  path.join(root, ".github", "workflows", "release-auto.yml"),
  "utf8",
);
// The installer downloads the whole -gnu ARCHIVE, not just soldr's own
// binary, so its floor must be the ceiling release-auto.yml enforces on the
// bundle (`verify_release_bundle.py --check glibc-baseline`), which also
// covers the prebuilt `crgx` / `cargo-chef` fetched from soldr-toolchain.
// Tracking the own-binary ceiling instead (2.17 since soldr#1060) would send
// glibc 2.17-2.38 hosts an archive whose bundled tools need 2.39.
const bundleCeiling = glibcCeilingFor(releaseWorkflow, "--check glibc-baseline");
assert(
  bundleCeiling,
  "release-auto.yml must pass --max-glibc to the bundled glibc-baseline check",
);
assert.strictEqual(
  install.MIN_GLIBC_FOR_GNU,
  bundleCeiling,
  `install.js MIN_GLIBC_FOR_GNU (${install.MIN_GLIBC_FOR_GNU}) must match the ` +
    `--max-glibc ceiling release-auto.yml enforces on the bundled -gnu archive ` +
    `(${bundleCeiling})`,
);

assert.strictEqual(
  install.checksumFor(
    "abc123  soldr-v0.7.29-x86_64-unknown-linux-gnu.tar.zst\n",
    "soldr-v0.7.29-x86_64-unknown-linux-gnu.tar.zst",
  ),
  "abc123",
);

// The integrity check for everything this package installs
// (docs/TRUST_BOUNDARIES.md). `checksumFor` was covered but the comparison
// that uses it was inline in install() and untested, so deleting the
// mismatch branch would have disabled verification with every test green.
const crypto = require("crypto");
const ARCHIVE_BYTES = Buffer.from("pretend this is a tar.zst");
const ARCHIVE_NAME = "soldr-v9.9.9-x86_64-unknown-linux-musl.tar.zst";
const ARCHIVE_SHA = crypto.createHash("sha256").update(ARCHIVE_BYTES).digest("hex");
const SUMS = `${ARCHIVE_SHA}  ${ARCHIVE_NAME}
`;

// A matching digest returns it rather than throwing.
assert.strictEqual(
  install.verifyArchiveChecksum(ARCHIVE_BYTES, SUMS, ARCHIVE_NAME),
  ARCHIVE_SHA,
);

// A tampered archive must throw. This is the case that had no coverage.
assert.throws(
  () =>
    install.verifyArchiveChecksum(
      Buffer.concat([ARCHIVE_BYTES, Buffer.from("tampered")]),
      SUMS,
      ARCHIVE_NAME,
    ),
  /checksum mismatch/,
  "a tampered archive must be rejected",
);

// A digest for a DIFFERENT file must not be accepted for this one: the
// lookup is by exact filename, so a sums file listing only some other
// artifact fails closed rather than matching the first line it sees.
assert.throws(
  () =>
    install.verifyArchiveChecksum(
      ARCHIVE_BYTES,
      `${ARCHIVE_SHA}  some-other-artifact.tar.zst
`,
      ARCHIVE_NAME,
    ),
  /checksum entry not found/,
  "a sums file without this artifact must be rejected",
);

// An empty sums file is not a free pass.
assert.throws(
  () => install.verifyArchiveChecksum(ARCHIVE_BYTES, "", ARCHIVE_NAME),
  /checksum entry not found/,
  "an empty checksums file must be rejected",
);

// Real SHA256SUMS.txt files use two spaces and may carry CRLF; both must
// parse. Verified against the published v0.8.29 file, which is
// `<hash>  <name>` with 16 entries covering every release asset.
assert.strictEqual(
  install.verifyArchiveChecksum(
    ARCHIVE_BYTES,
    `deadbeef  unrelated.whl
${ARCHIVE_SHA}  ${ARCHIVE_NAME}
`,
    ARCHIVE_NAME,
  ),
  ARCHIVE_SHA,
);

// Digests are compared case-insensitively on the manifest side: an
// uppercase entry names the same bytes.
assert.strictEqual(
  install.verifyArchiveChecksum(
    ARCHIVE_BYTES,
    `${ARCHIVE_SHA.toUpperCase()}  ${ARCHIVE_NAME}
`,
    ARCHIVE_NAME,
  ),
  ARCHIVE_SHA,
);

assert.strictEqual(install.ARCHIVE_EXT, "tar.zst");
assert.deepStrictEqual(
  install.BUNDLED_BINARIES,
  zccacheContract.RELEASE_BUNDLED_BINARIES,
);
// zccache is embedded into soldr/soldr-daemon; no standalone zccache
// binaries are bundled or downloaded.
assert.deepStrictEqual(zccacheContract.ZCCACHE_BUNDLED_BINARIES, []);
assert.strictEqual(zccacheContract.CRGX_BUNDLED_BINARY, "crgx");
assert.strictEqual(zccacheContract.CARGO_CHEF_BUNDLED_BINARY, "cargo-chef");
assert.deepStrictEqual(
  zccacheContract.soldrDebugInfoEntries({
    soldr: {
      debug_info: [{ name: "soldr.pdb", sha256: "0".repeat(64), format: "pdb" }],
    },
  }),
  [{ name: "soldr.pdb", sha256: "0".repeat(64), format: "pdb" }],
);
assert.ok(
  fs.existsSync(path.join(root, "contracts", "zccache-integration-guardrails.v1.json")),
  "npm package must include the zccache integration guardrail contract",
);

// Every TARGETS entry must drop the `archive` field — the combined
// archive format is fixed (.tar.zst) so the per-target field is dead
// data. Catch a regression early if anyone re-adds it.
for (const [key, target] of Object.entries(install.TARGETS || {})) {
  assert.ok(
    typeof target.triple === "string" && target.triple.length > 0,
    `TARGETS[${key}].triple must be a non-empty string`,
  );
  assert.ok(
    typeof target.binary === "string" && target.binary.length > 0,
    `TARGETS[${key}].binary must be a non-empty string`,
  );
  assert.strictEqual(
    target.archive,
    undefined,
    `TARGETS[${key}].archive must be removed — archive format is fixed at .tar.zst`,
  );
}

// BUNDLED_BINARIES must include soldr, soldr-daemon, crgx, and
// cargo-chef. It must not require standalone zccache binaries.
assert.ok(
  Array.isArray(install.BUNDLED_BINARIES),
  "install.BUNDLED_BINARIES must be exported as an array",
);
for (const required of [
  "soldr",
  "soldr-daemon",
  "crgx",
  "cargo-chef",
]) {
  assert.ok(
    install.BUNDLED_BINARIES.includes(required),
    `BUNDLED_BINARIES must include "${required}" (got ${JSON.stringify(install.BUNDLED_BINARIES)})`,
  );
}

// installFirstWorkingCandidate's retry policy: it must try candidates in
// order, stay quiet on the common case, and when it does fall back, the log
// has to say which artifact failed, why, and which one was installed
// instead -- otherwise a user staring at a successful install with the
// "wrong" binary has no way to find out why.
async function checkCandidateFallback() {
  const gnu = { triple: "x86_64-unknown-linux-gnu", binary: "soldr" };
  const musl = { triple: "x86_64-unknown-linux-musl", binary: "soldr" };
  const collector = () => {
    const lines = [];
    return { lines, warn: (line) => lines.push(line) };
  };

  // 1. First candidate succeeds -> it is returned, attempt ran exactly once,
  //    and nothing is logged. A normal install must stay quiet; logging a
  //    "warning" on the success path would train users to ignore warnings.
  {
    const log = collector();
    const attempts = [];
    const result = await install.installFirstWorkingCandidate(
      [gnu, musl],
      async (target) => {
        attempts.push(target.triple);
        return target;
      },
      log,
    );
    assert.deepStrictEqual(result, gnu);
    assert.deepStrictEqual(attempts, [gnu.triple]);
    assert.deepStrictEqual(log.lines, []);
  }

  // 2. First candidate fails, sibling succeeds -> resolves with the sibling,
  //    attempts happened in order (gnu before musl -- it must not try the
  //    sibling first), and the log names both the failed triple and its
  //    reason, plus a line reporting the sibling that was actually
  //    installed. This is the RFC's whole point: the log must say what
  //    happened and why.
  {
    const log = collector();
    const attempts = [];
    const result = await install.installFirstWorkingCandidate(
      [gnu, musl],
      async (target) => {
        attempts.push(target.triple);
        if (target.triple === gnu.triple) {
          throw new Error("HTTP 404");
        }
        return target;
      },
      log,
    );
    assert.deepStrictEqual(result, musl);
    assert.deepStrictEqual(attempts, [gnu.triple, musl.triple]);
    assert.ok(
      log.lines.some((l) => l.includes("x86_64-unknown-linux-gnu") && l.includes("HTTP 404")),
      "fallthrough must log the failed triple and the reason",
    );
    // Must be a line that says something was INSTALLED, naming both what was
    // installed and what was preferred. Matching the musl triple alone is not
    // enough: the fallthrough warning already names it, so that weaker check
    // would still pass if the success line were deleted entirely.
    assert.ok(
      log.lines.some(
        (l) =>
          l.includes("installed") &&
          l.includes("x86_64-unknown-linux-musl") &&
          l.includes("x86_64-unknown-linux-gnu"),
      ),
      `a non-first success must log what was installed and what was preferred: ${log.lines.join(" | ")}`,
    );
  }

  // 3. Both candidates fail -> the promise rejects, and the rejection names
  //    every triple and every reason. A fallback that swallows the original
  //    error on the way to the final rejection makes the failure
  //    unreportable -- exactly the failure mode this feature exists to fix.
  {
    const log = collector();
    const promise = install.installFirstWorkingCandidate(
      [gnu, musl],
      async (target) => {
        if (target.triple === gnu.triple) {
          throw new Error("HTTP 404");
        }
        throw new Error("checksum mismatch");
      },
      log,
    );
    await assert.rejects(promise, /no soldr release artifact could be installed/);
    await assert.rejects(
      promise,
      (err) =>
        err.message.includes("x86_64-unknown-linux-gnu") &&
        err.message.includes("x86_64-unknown-linux-musl") &&
        err.message.includes("HTTP 404") &&
        err.message.includes("checksum mismatch"),
    );
    // Exactly one log line: the gnu -> musl fallthrough. Nothing may claim an
    // install happened when every candidate failed.
    assert.strictEqual(log.lines.length, 1, `unexpected log lines: ${log.lines.join(" | ")}`);
    assert.ok(
      !log.lines.some((l) => l.includes("installed")),
      "an all-candidates-failed run must not log that anything was installed",
    );
  }

  // 4. Single-candidate (the non-Linux shape) failure -> rejects, attempted
  //    exactly once, and nothing is logged. There is no sibling to fall back
  //    to, so nothing may claim a fallback happened.
  {
    const log = collector();
    const attempts = [];
    const promise = install.installFirstWorkingCandidate(
      [{ triple: "aarch64-apple-darwin", binary: "soldr" }],
      async (target) => {
        attempts.push(target.triple);
        throw new Error("HTTP 404");
      },
      log,
    );
    await assert.rejects(promise, /no soldr release artifact could be installed/);
    assert.strictEqual(attempts.length, 1);
    assert.deepStrictEqual(log.lines, []);
  }

  // 4b. An empty candidate list is rejected with a message that says so,
  //     rather than falling through the loop to "could not be installed ()"
  //     with an empty reason list. platformCandidates throws instead of
  //     returning one, so this only fires for a hand-built list.
  {
    const log = collector();
    let attempted = false;
    await assert.rejects(
      install.installFirstWorkingCandidate([], async () => {
        attempted = true;
      }, log),
      /empty candidate list/,
    );
    assert.strictEqual(attempted, false);
    assert.deepStrictEqual(log.lines, []);
  }

  // 5. Real-shape wiring: platformCandidates feeding straight into
  //    installFirstWorkingCandidate resolves with the sibling when the
  //    preferred triple fails, proving the ordering helper and the retry
  //    loop compose the way install() actually uses them.
  {
    const log = collector();
    const candidates = install.platformCandidates("linux", "x64", "musl");
    const result = await install.installFirstWorkingCandidate(
      candidates,
      async (target) => {
        if (target.triple === "x86_64-unknown-linux-musl") {
          throw new Error("HTTP 404");
        }
        return target;
      },
      log,
    );
    assert.strictEqual(result.triple, "x86_64-unknown-linux-gnu");
  }
}

checkCandidateFallback()
  .then(() => {
    console.log("npm package and PyPI version checks passed");
  })
  .catch((error) => {
    console.error(error && error.stack ? error.stack : error);
    process.exit(1);
  });
