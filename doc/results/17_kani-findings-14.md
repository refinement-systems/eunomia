# Kani verification findings — part 14 (CI cover post-check message)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `16_kani-findings-13.md`.
This part implements **recommendation #5** of the second conformance review
(`14_kani-review-2.md`): tighten the CI `kani`-job cover post-check so its failure
message distinguishes "no cover lines found" from "a cover went unreachable."
**CI-only — no harness, no kcore code, no proof defect** — a fail-closed-message
robustness fix. The standing caveat and design notes (DN-1…DN-13) of the earlier
parts apply unchanged; this adds no new DN.

## The bug (review-2 critique 4)

The vacuity guard (finding DN-13, `11_kani-findings-9.md`) exists because Kani
0.67 treats `kani::cover!` as *informational*: an unreachable cover does not fail
the run, it only lowers the per-harness `N of M cover properties satisfied` tally.
So the CI job greps those tally lines out of the Kani logs and fails if any has
`N < M`. The check was:

```bash
set -o pipefail
if grep -hE 'cover properties satisfied' kani-*.log \
     | awk '{ if ($2 != $4) { print "UNREACHABLE cover: " $0; bad=1 } } END { exit bad+0 }'; then
  echo "all kani::cover! checkpoints satisfied"
else
  echo "::error::a kani::cover! checkpoint was UNREACHABLE — a proof may be vacuous (rec #3)"
  exit 1
fi
```

With `set -o pipefail`, **two unrelated failures collapse onto the same message**:

1. `awk` sets `bad=1` because some line has `$2 != $4` — a cover genuinely went
   unreachable. The "UNREACHABLE" message is correct.
2. `grep` matches **zero** tally lines — a Kani output-format change, or a log
   that never wrote — so `grep` exits non-zero, `pipefail` propagates that through
   the pipe, and the `else` branch fires the **same "UNREACHABLE cover"** message.
   That is wrong: there were no tallies to be unreachable. Failing closed is the
   right direction, but the message sends a future on-call reader hunting for a
   vacuous proof when the real cause is a missing or reformatted log.

## The fix

Capture the grep output once (`|| true` so a zero-match grep does not trip
`pipefail`/`-e` before the explicit test), branch on emptiness, then run the
unchanged `N != M` awk over the captured lines:

```bash
covers=$(grep -hE 'cover properties satisfied' kani-*.log || true)
if [ -z "$covers" ]; then
  echo "::error::no 'cover properties satisfied' lines in the kani logs — Kani's output format may have changed or a log failed to write; the vacuity guard could not run (rec #5)"
  exit 1
fi
if printf '%s\n' "$covers" \
     | awk '{ if ($2 != $4) { print "UNREACHABLE cover: " $0; bad=1 } } END { exit bad+0 }'; then
  echo "all kani::cover! checkpoints satisfied"
else
  echo "::error::a kani::cover! checkpoint was UNREACHABLE — a proof may be vacuous (rec #3)"
  exit 1
fi
```

`grep -h` is kept (it strips the filename prefixes across `kani-*.log`, so awk's
`$2`/`$4` stay the numeric fields), as is the per-line `UNREACHABLE cover:`
diagnostic. The change is confined to `.github/workflows/ci.yml`; `kani-deep.yml`
has no cover post-check, so no other workflow is touched.

**Both modes still fail closed** — the guard never silently passes. Only the
message differs, and now it names the actual condition.

## Verification

The GitHub Action can't be run here, but the bash/awk *is* the whole logic, so it
was exercised locally against four crafted log sets:

| Case | Input | Result |
|---|---|---|
| all satisfied | `** 41 of 41 …`, `** 3 of 3 …` | `all … satisfied`, exit 0 |
| cover unreachable | a `** 40 of 41 …` line | `UNREACHABLE cover: …` + rec-#3 error, exit 1 |
| no tally lines (log exists) | log without a tally line | rec-#5 "no cover lines" error, exit 1 |
| logs missing (glob no-match) | empty dir | rec-#5 error, exit 1 |

The two formerly-conflated cases (rows 3–4) now report the rec-#5 message instead
of the misleading "UNREACHABLE"; a real unreachable cover (row 2) still reports
the rec-#3 message. `ci.yml` re-parses as valid YAML. No Kani or host re-run is
required (no kcore/harness change), consistent with the docs-only precedent of
parts 11 and 13.

## Status of recommendation #5

✅ Done. The cover post-check now distinguishes an absent tally from an
unreachable cover, both failing closed. With this, **all five of the review's
*routine* recommendations (#1–#5) are landed.** The only remaining review-2 item
is **#6** — a time-boxed `-Z function-contracts` spike on `revoke`/`obj_unref`,
explicitly kept *off* the pinned CI path as research (unstable Kani surface), the
one route by which the bounded teardown/revoke results could become unbounded
proofs.
