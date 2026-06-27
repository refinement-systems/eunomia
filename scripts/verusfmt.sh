#!/bin/bash
#
# Format Verus code with verusfmt — the complement to `cargo fmt`.
#
# `cargo fmt` (rustfmt) does not format code INSIDE the `verus!{}` macro;
# verusfmt does. We run verusfmt in `--verus-only` mode so it touches only the
# macro interior and leaves every out-of-macro line to `cargo fmt`, which stays
# the authoritative formatter (see CLAUDE.md "Formatting"). The two tools then
# have disjoint domains, and `verusfmt --verus-only` followed by `cargo fmt` is
# a deterministic fixed point. verusfmt is layout-only and does not change what
# Verus proves (the verification gate was re-run green over the swept tree).
#
# Usage:
#   scripts/verusfmt.sh            format in place, then run `cargo fmt`
#   scripts/verusfmt.sh --check    verify formatting only (CI / pre-commit gate)
#
# Excluded files — verusfmt 0.7.2 mishandles these, so they keep their hand
# formatting and `cargo fmt` alone owns them:
#   cas/src/disk.rs, cas/src/prolly.rs    verusfmt's parser rejects the
#                                         half-open range index `x[..n]`.
#   cas/src/store.rs, kcore/src/aspace.rs files with several `verus!{}` blocks:
#                                         verusfmt wrongly indents the plain-Rust
#                                         comments BETWEEN the blocks.
#   eunomia-sys/src/bootstrap.rs,         files with NO `verus!{}` macro block —
#   eunomia-sys/src/io_error.rs           they only name the token in a doc
#                                         comment, but `git grep -l 'verus!'`
#                                         still selects them, and verusfmt then
#                                         reformats their plain-Rust layout
#                                         (e.g. dropping the blank line after the
#                                         module doc) against `cargo fmt`.
# If a new file gains any of these traits (a `x[..n]` index, comments between
# multiple `verus!{}` blocks that verusfmt re-indents, or only a comment mention
# of the macro and no real block), add it here.
#
# This script covers the root-workspace Verus crates. No `user/*` binary or
# `*/fuzz` crate contains `verus!{}`, so the separate-workspace `cargo fmt`
# caveat in CLAUDE.md does not apply to verusfmt.
set -euo pipefail
cd "$(dirname "$0")/.."

# Files verusfmt cannot format correctly (see header).
SKIP="cas/src/disk.rs cas/src/prolly.rs cas/src/store.rs kcore/src/aspace.rs eunomia-sys/src/bootstrap.rs eunomia-sys/src/io_error.rs"

is_skipped() {
	case " $SKIP " in
	*" $1 "*) return 0 ;;
	*) return 1 ;;
	esac
}

mode="${1:-fix}"
rc=0

# All tracked, non-vendored sources that use the macro.
files="$(git grep -l 'verus!' -- '*.rs' | grep -v '^vendor/')"

if [ "$mode" = "--check" ]; then
	while IFS= read -r f; do
		[ -n "$f" ] || continue
		is_skipped "$f" && continue
		if ! verusfmt --verus-only --check "$f" >/dev/null 2>&1; then
			echo "verusfmt: $f is not formatted (run scripts/verusfmt.sh)"
			rc=1
		fi
	done <<<"$files"
	# cargo fmt is the authority for all out-of-macro code.
	cargo fmt --check || rc=1
	exit "$rc"
fi

while IFS= read -r f; do
	[ -n "$f" ] || continue
	is_skipped "$f" && continue
	verusfmt --verus-only "$f" >/dev/null
done <<<"$files"

# Settle out-of-macro code (and the skipped/non-Verus crates) with cargo fmt.
cargo fmt
