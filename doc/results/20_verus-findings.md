# Verus finding 20 — `content_ok_spec` opaque measurement (phase 6.7)

## Summary

Measured whether `#[verifier::opaque]` on `content_ok_spec` (`cas/src/store.rs`) helps or
hurts per the plan (phase 6.7). Removal significantly regresses the five recursive consumers
named in the task; the opaque is retained.

## Setup

- Verus `0.2026.06.07.cd03505`, toolchain `1.95.0-aarch64-apple-darwin`
- Cold runs: `cargo clean -p cas && cargo verus verify -p cas --no-default-features -- --time-expanded --output-json`
- Both runs: `79 verified, 0 errors`

## Per-function rlimit

| Function | With opaque (baseline) | Without opaque | Δ |
|---|---|---|---|
| `cas::store::run_len` | 34,029 | 47,156 | +38.6% |
| `cas::store::laid_out` | 2,841 | 2,841 | flat |
| `cas::store::recover_records` | 284,385 | 477,427 | +67.9% |
| `cas::store::lemma_recover_reconstructs` | 6,344 | 6,333 | flat |
| `cas::store::lemma_gap_freedom` | 24,881 | 33,686 | +35.3% |
| `cas::store::lemma_run_len_covers` | 62,038 | 97,013 | +56.4% |
| `cas::store::lemma_forall_laid_out` | 23,111 | 34,374 | +48.6% |
| `cas::store::lemma_laid_out_mono` | 18,542 | 27,704 | +49.5% |
| `cas::store::lemma_recover_reconstructs_pins_head` | 5,357 | 5,853 | +9.3% |
| **CRATE TOTAL** | **15,403,513** | **16,341,678** | **+6.1%** |

## Decision

**Keep the opaque.** Three of the five plan-named consumers regress materially
(`run_len` +39%, `recover_records` +68%, `lemma_gap_freedom` +35%) and the crate total rises
6.1%. Only `laid_out` and `lemma_recover_reconstructs` are flat. The condition for dropping
the opaque (flat-or-better on *every* listed consumer) is not met.

The opaque functions as a recursive-structural-decode shield: `content_ok_spec` is
non-recursive but its body pulls in the recursive `s_payload_ok`/`s_path` family, and its
consumers (`run_len`, `laid_out`) are themselves recursive. The measurement confirms the
rationale in the existing comment at `store.rs:870-872`.

No code change; `store.rs` is byte-identical to the pre-measurement tree.
