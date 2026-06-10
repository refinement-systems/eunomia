---- MODULE CommitProtocol ----
\* Storage commit / crash-recovery protocol for Eunomia OS (spec §4.2–4.5).
\*
\* State to model:
\*   - Two superblock slots A and B (one valid at a time, flipped on commit)
\*   - Per-slot: generation counter, ref-table root hash, WAL head position,
\*               checksum-valid flag (true = not torn)
\*   - WAL: a sequence of (refId, overlayRoot) records; each record is
\*          either "flushed" (its effects are in the chunk store) or not
\*   - Per-ref overlay: unflushed (dirty) or clean
\*   - A global "crashed" flag to model crash at any point
\*
\* Key invariant to check (spec §4.5):
\*   After any crash followed by recovery, the recovered state equals:
\*     { committed ref roots from winning superblock }
\*     UNION
\*     { overlay reconstructed by replaying WAL records whose position
\*       is > winning-slot.walHead }
\*
\* Partial-flush invariant:
\*   A commit may carry new roots for a subset of refs; the others retain
\*   their previous committed roots. A crash mid-commit leaves the previous
\*   slot intact and the new slot with a bad checksum — recovery discards it.
\*
\* Actions to model:
\*   Write(ref, data)        - lands in ref's overlay
\*   WalAppend(ref)          - fsync overlay record to WAL
\*   Flush(ref)              - freeze overlay, write chunks, path-copy tree
\*   Commit(refSet)          - fsync chunks, write new superblock, fsync it
\*   Crash                   - set crashed flag at any point
\*   Recover                 - read both slots, pick higher-gen valid one,
\*                             replay WAL tail
\*
\* This spec must be model-checked (TLC) before M2 implementation begins.

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Refs,    \* set of ref identifiers, e.g. {"main"}
    NULL     \* sentinel for "no parent" / empty

VARIABLES
    slotA,       \* superblock slot A
    slotB,       \* superblock slot B
    walLog,      \* sequence of WAL records: <<refId, generation>>
    overlays,    \* Refs -> {"clean", "dirty"}
    crashed,     \* TRUE when simulating crash state
    recovering   \* TRUE during recovery execution

vars == <<slotA, slotB, walLog, overlays, crashed, recovering>>

\* A superblock record.
Superblock(gen, refRoots, walHead, valid) ==
    [generation |-> gen,
     refRoots   |-> refRoots,
     walHead    |-> walHead,
     valid      |-> valid]

EmptyRoots == [r \in Refs |-> NULL]

Init ==
    /\ slotA    = Superblock(0, EmptyRoots, 0, TRUE)
    /\ slotB    = Superblock(0, EmptyRoots, 0, FALSE)
    /\ walLog   = << >>
    /\ overlays = [r \in Refs |-> "clean"]
    /\ crashed  = FALSE
    /\ recovering = FALSE

\* Helper: the live (highest-generation valid) superblock.
LiveSlot ==
    IF slotA.valid /\ slotB.valid
    THEN IF slotA.generation >= slotB.generation THEN slotA ELSE slotB
    ELSE IF slotA.valid THEN slotA
    ELSE slotB

\* --- Actions (stubs — fill in before M2) ---

Write(r) ==
    /\ ~crashed
    /\ overlays[r] = "clean"
    /\ overlays' = [overlays EXCEPT ![r] = "dirty"]
    /\ UNCHANGED <<slotA, slotB, walLog, crashed, recovering>>

\* TODO: model Flush, WalAppend, Commit, Crash, Recover

Next ==
    \/ \E r \in Refs : Write(r)
    \* \/ Flush(r) \/ Commit(...) \/ Crash \/ Recover

Spec == Init /\ [][Next]_vars

\* --- Invariants ---

\* After recovery, every ref's root in the live slot is either NULL (never
\* committed) or was explicitly committed; overlays reflect WAL replay.
\* (Full invariant body deferred to M2 spec work.)
TypeOK ==
    /\ slotA.generation \in Nat
    /\ slotB.generation \in Nat
    /\ Len(walLog) \in Nat
    /\ \A r \in Refs : overlays[r] \in {"clean", "dirty"}

====
