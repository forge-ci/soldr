#!/bin/sh
# Nextest run-wrapper entry point (soldr#3453, soldr#3454).
#
# Every Unix test runs through the native `soldr-nextest-wrapper`
# (crates/soldr-nextest-wrapper), the one implementation of the wrapper
# contract. This shim only finds it, by one rule:
#
#   1. $SOLDR_NEXTEST_NATIVE_WRAPPER, when set and trusted (an explicit
#      override, see the isolation note below; the black-box suites use their
#      own SOLDR_NEXTEST_WRAPPER_UNDER_TEST);
#   2. otherwise beside the test binary's Cargo profile directory:
#      `<target>/[<triple>/]<profile>/deps/<test>` ->
#      `<target>/[<triple>/]<profile>/soldr-nextest-wrapper`. The first
#      argument under a `deps/` directory is the test binary, even when a
#      target runner precedes it. A `--workspace` build puts the wrapper
#      there, and `nextest archive` ships it at the same relative path as a
#      non-test binary, so ci-test and extracted archives (target-run lanes,
#      the macOS Recovery replay) need nothing more;
#   3. a scoped run (`nextest run -p soldr-cli ...`) never builds the
#      wrapper's package, so on a miss the first test builds it, once, into
#      exactly that directory: profile and triple come from the same path
#      (`debug` is Cargo's `test` profile; a triple-shaped directory under a
#      visible Cargo target root is a `--target` triple), and the build uses
#      the run's own Cargo (`$CARGO`, never a bare `cargo`; soldr#2878) under
#      an atomic mkdir lock so concurrent first tests wait for one build.
#      Nextest setup scripts cannot do this: they are given no target dir,
#      Cargo profile or triple (nextest 0.9.140), which only this path names;
#   4. otherwise -- no `deps/` path, no `$CARGO` (an archive host, where
#      Cargo is absent and a build must not be attempted), or a failed build
#      -- refuse, loudly: a test never runs unwrapped.
#
# POSIX sh and parameter expansion only -- no dirname, flock or python3: the
# macOS Recovery guest has none of them guaranteed.
#
# zackees/ci.yml#168 (GATE-005), soldr#3516: soldr's tests start soldr
# brokers and daemons, and a leaked fixture daemon once claimed the real
# ~/.soldr root and wedged every soldr build on the machine. The native
# wrapper this shim execs is what makes that impossible: it points HOME,
# SOLDR_CACHE_DIR, the XDG directories and XDG_RUNTIME_DIR of every test into
# a private per-test root, refuses (exit 97) when it cannot, and exports
# SOLDR_TEST_ISOLATED=1 to the test (crates/soldr-nextest-wrapper/src/
# isolate.rs). So the suite is safe to run on a developer host, and the local
# gate's tests lane runs it there (ci/workspace_test.py --host).
#
# The one way around that is to replace the native wrapper. An explicit
# SOLDR_NEXTEST_NATIVE_WRAPPER is therefore honoured only where a replacement
# cannot reach a developer's state: under CI=true (a disposable runner or act
# job), or with SOLDR_TEST_ISOLATED=1 (inside a test the native wrapper has
# already isolated, or a disposable guest that sets it: the macOS Recovery
# VM). Anywhere else it is refused rather than allowed to run tests unwrapped.
if [ -n "${SOLDR_NEXTEST_NATIVE_WRAPPER:-}" ]; then
    if [ "${CI:-}" != "true" ] && [ "${SOLDR_TEST_ISOLATED:-}" != "1" ]; then
        echo "nextest_wrapper.sh: refusing SOLDR_NEXTEST_NATIVE_WRAPPER=$SOLDR_NEXTEST_NATIVE_WRAPPER on a developer host (zackees/ci.yml#168, soldr#3516):" >&2
        echo "  only soldr-nextest-wrapper isolates a test's soldr state; unset it, or run the suite through ci/workspace_test.py --host" >&2
        exit 97
    fi
    exec "$SOLDR_NEXTEST_NATIVE_WRAPPER" "$@"
fi
NAME=soldr-nextest-wrapper

refuse() {
    echo "nextest_wrapper.sh: $NAME unavailable (soldr#3454): $1" >&2
    echo "  test command: $*" >&2
    echo "  build it with the tests (select the $NAME package, or" >&2
    echo "  soldr cargo build -p $NAME with the same --target/--profile)," >&2
    echo "  or name it in SOLDR_NEXTEST_NATIVE_WRAPPER. Tests never run unwrapped." >&2
    exit 98
}

profile_dir=""
for arg in "$@"; do
    parent="${arg%/*}"
    case "$parent" in
    */deps)
        profile_dir="${parent%/*}"
        break
        ;;
    esac
done
[ -n "$profile_dir" ] || refuse "no test binary under a deps/ directory in the argv" "$@"
wrapper="$profile_dir/$NAME"
[ -x "$wrapper" ] && exec "$wrapper" "$@"

# Step 3: build it once into $profile_dir.
[ -n "${CARGO:-}" ] || refuse "not at $wrapper, and no \$CARGO to build it with" "$@"
profile="${profile_dir##*/}"
[ "$profile" = debug ] && profile=test
above="${profile_dir%/*}"
target_root="$above"
triple=""
# `<root>/<triple>/<profile>` vs `<root>/<profile>`: a triple-shaped parent
# counts only when the directory above it is visibly a Cargo target root --
# its rustc info cache, its CACHEDIR.TAG, or the host-side `<profile>` dir a
# `--target` build puts build scripts and proc-macros in.
case "${above##*/}" in
*-*-*)
    root_candidate="${above%/*}"
    if [ -f "$root_candidate/.rustc_info.json" ] || [ -f "$root_candidate/CACHEDIR.TAG" ] ||
        [ -d "$root_candidate/${profile_dir##*/}" ]; then
        target_root="$root_candidate"
        triple="${above##*/}"
    fi
    ;;
esac
lock="$profile_dir/.$NAME.build-lock"
waited=0
until mkdir "$lock" 2>/dev/null; do # stdio-ok: "exists" is the expected answer while another test holds the lock
    [ -x "$wrapper" ] && exec "$wrapper" "$@"
    owner=""
    [ -f "$lock/pid" ] && read -r owner <"$lock/pid"
    if [ -n "$owner" ] && ! kill -0 "$owner" 2>/dev/null; then # stdio-ok: liveness probe; "no such process" is the signal
        rm -rf "$lock" # its builder died; take over
        continue
    fi
    waited=$((waited + 1))
    [ "$waited" -le 1800 ] || refuse "timed out waiting for another test's build ($lock)" "$@"
    sleep 1
done
echo $$ >"$lock/pid"
if [ ! -x "$wrapper" ]; then
    echo "nextest_wrapper.sh: building $NAME into $profile_dir (soldr#3454)" >&2
    if [ -n "$triple" ]; then
        "$CARGO" build -p "$NAME" --bin "$NAME" --profile "$profile" \
            --target "$triple" --target-dir "$target_root" 1>&2
    else
        "$CARGO" build -p "$NAME" --bin "$NAME" --profile "$profile" \
            --target-dir "$target_root" 1>&2
    fi
fi
rm -rf "$lock"
[ -x "$wrapper" ] || refuse "building it with $CARGO did not produce $wrapper" "$@"
exec "$wrapper" "$@"
