#!/bin/sh
set -eu

# The activation script ships the working tree straight to production, and
# until now nothing observed it: no test, no gate, no shellcheck. Its pinned
# commit rotted three times in one pull request and each break surfaced only
# when an operator tried to activate. Everything asserted here runs against a
# throwaway fixture repository and stops at a stubbed gate, so no case reaches
# Wrangler, a credential, or the network.

repository_root=$(CDPATH='' cd -- "$(/usr/bin/dirname "$0")/.." && pwd -P)
temporary=$(/usr/bin/mktemp -d "${TMPDIR:-/tmp}/dark-factory-bootstrap-test.XXXXXX")
trap '/bin/rm -rf "$temporary"' EXIT
trap 'exit 1' HUP INT TERM

fail() {
    echo "bootstrap activation test failed: $*" >&2
    exit 1
}

fixture="$temporary/repository"
/bin/mkdir -p "$fixture/scripts" "$fixture/control-plane/scripts" \
    "$fixture/control-plane/src"

# The activation derives the permission revision from this constant, inside the
# tree it just proved. The fixture carries it so the cases below exercise the
# script's own parse rather than a copy of it restated in this test.
revision_line='pub(crate) const PERMISSION_REVISION: &str = "maintainer-operations-v6";'
# The neighbouring constant must not match. It shares the prefix and differs
# only after it, which is the whole risk in the pattern.
{
    echo 'pub(crate) const PERMISSION_REVISION_BINDING: &str = "DARK_FACTORY_MAINTAINER_PERMISSION_REVISION";'
    echo "$revision_line"
} >"$fixture/control-plane/src/github_app.rs"
/bin/cp "$repository_root/scripts/bootstrap-maintainer-v2.sh" "$fixture/scripts/"
/bin/chmod 755 "$fixture/scripts/bootstrap-maintainer-v2.sh"

# The authoritative gate stands between the source check and every credential
# use, so a run that reaches this stub has passed the source gate and has not
# deployed. Exit 3 is distinguishable from the script's own exit 1.
cat >"$fixture/control-plane/scripts/local-ci.sh" <<'STUB'
#!/bin/sh
echo "STUB-GATE-REACHED"
exit 3
STUB
/bin/chmod 755 "$fixture/control-plane/scripts/local-ci.sh"

git=/usr/bin/git
"$git" -C "$fixture" init -q
"$git" -C "$fixture" config user.name fixture
"$git" -C "$fixture" config user.email fixture@example.invalid
"$git" -C "$fixture" add -A
"$git" -C "$fixture" -c commit.gpgsign=false commit -qm fixture
tree=$("$git" -C "$fixture" rev-parse HEAD:control-plane)

run() {
    ( cd "$fixture" && ./scripts/bootstrap-maintainer-v2.sh "$@" >"$temporary/out" 2>&1 )
}

expect_refusal() {
    expected_text=$1
    shift
    status=0
    run "$@" || status=$?
    test "$status" = 1 || fail "expected exit 1 for [$*], got $status"
    /usr/bin/grep -Fq "$expected_text" "$temporary/out" \
        || fail "expected [$*] to report '$expected_text', got: $(cat "$temporary/out")"
    if /usr/bin/grep -Fq STUB-GATE-REACHED "$temporary/out"; then
        fail "[$*] passed the source gate but should not have"
    fi
}

# A missing or malformed tree is refused before any repository state is read.
expect_refusal "usage: scripts/bootstrap-maintainer-v2.sh"
expect_refusal "usage: scripts/bootstrap-maintainer-v2.sh" "$tree" extra
expect_refusal "must be a full 40-character SHA-1" abc123
expect_refusal "must be a full 40-character SHA-1" recover-v1
expect_refusal "must be lowercase hex" zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz
expect_refusal "must be lowercase hex" ABCDEF0123456789ABCDEF0123456789ABCDEF01

# A well-formed tree that is not the one at HEAD names both values rather than
# claiming the runtime is unreviewed, which is what the previous commit-pinned
# gate did when its pin went stale.
expect_refusal "not the reviewed 1111111111111111111111111111111111111111" \
    1111111111111111111111111111111111111111
/usr/bin/grep -Fq "control-plane at HEAD is $tree" "$temporary/out" \
    || fail "a tree mismatch must name the tree actually at HEAD"

# An uncommitted change means HEAD is not what would ship, so the tree the
# operator named proves nothing. Untracked counts: it would be uploaded.
: >"$fixture/control-plane/stowaway"
expect_refusal "activation script or control-plane is dirty" "$tree"
/bin/rm "$fixture/control-plane/stowaway"

# `HEAD:control-plane` is not self-evidently a tree. A symlinked path resolves
# to a blob, and the bytes that would ship then sit outside the proven object.
symlinked="$temporary/symlinked"
/bin/cp -R "$fixture" "$symlinked"
test -d "$symlinked/.git" || fail "the symlink fixture is not its own repository"
( cd "$symlinked" && /bin/rm -rf control-plane && /bin/mkdir -p real/scripts \
    && /bin/cp "$fixture/control-plane/scripts/local-ci.sh" real/scripts/ \
    && /bin/ln -s real control-plane )
"$git" -C "$symlinked" add -A
"$git" -C "$symlinked" -c commit.gpgsign=false commit -qm symlink
blob=$("$git" -C "$symlinked" rev-parse HEAD:control-plane)
if ( cd "$symlinked" && ./scripts/bootstrap-maintainer-v2.sh "$blob" >"$temporary/out" 2>&1 ); then
    fail "a symlinked control-plane reached the gate"
fi
/usr/bin/grep -Fq "is not a tree object" "$temporary/out" \
    || fail "a non-tree control-plane must be refused as such, got: $(cat "$temporary/out")"

# The activation script is source too, and a half-applied edit to it must not
# ship any more than a half-applied edit to the runtime.
echo "# stray" >>"$fixture/scripts/bootstrap-maintainer-v2.sh"
expect_refusal "activation script or control-plane is dirty" "$tree"
"$git" -C "$fixture" checkout -- scripts/bootstrap-maintainer-v2.sh

# The exact reviewed tree on a clean checkout reaches the authoritative gate,
# and stops there. Nothing beyond this point runs without the gate passing.
status=0
run "$tree" || status=$?
test "$status" = 3 || fail "expected the stubbed gate's exit 3, got $status"
/usr/bin/grep -Fq STUB-GATE-REACHED "$temporary/out" \
    || fail "the exact reviewed tree did not reach the authoritative gate"

# A constant the activation cannot parse must stop the run before the gate,
# not surface as a Worker whose own authority check rejects it. Each shape is
# committed into the fixture so the script's real expression decides.
reject_revision() {
    /bin/cp "$fixture/control-plane/src/github_app.rs" "$temporary/github_app.rs.bak"
    printf '%s\n' "$1" >"$fixture/control-plane/src/github_app.rs"
    "$git" -C "$fixture" -c commit.gpgsign=false commit -qam "revision case"
    broken=$("$git" -C "$fixture" rev-parse HEAD:control-plane)
    status=0
    run "$broken" || status=$?
    test "$status" = 1 || fail "expected exit 1 for revision case [$2], got $status"
    /usr/bin/grep -Fq "exactly one PERMISSION_REVISION" "$temporary/out" \
        || fail "revision case [$2] gave: $(cat "$temporary/out")"
    /bin/cp "$temporary/github_app.rs.bak" "$fixture/control-plane/src/github_app.rs"
    "$git" -C "$fixture" -c commit.gpgsign=false commit -qam "restore revision"
}

reject_revision 'pub(crate) const PERMISSION_REVISION_BINDING: &str = "X";' absent
reject_revision 'pub(crate) const PERMISSION_REVISION: &str =
    "maintainer-operations-v6";' rustfmt-wrapped
reject_revision 'pub(crate) const PERMISSION_REVISION: &str = "";' empty
reject_revision 'pub(crate) const PERMISSION_REVISION: &str = "a";
pub(crate) const PERMISSION_REVISION: &str = "b";' duplicated

echo "bootstrap activation test passed"
