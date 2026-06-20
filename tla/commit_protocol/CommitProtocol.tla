---- MODULE CommitProtocol ----
\* Storage commit / crash-recovery protocol for Eunomia OS (spec rev1§4.2–4.5, rev1§6).
\*
\* Abstraction: writes to a ref are numbered 1,2,3,… per ref ("versions").
\* Content is last-write-wins (rev1§4.4), so the tree flushed at version v
\* contains the effects of every write ≤ v of that ref. "Record <<r, u>>
\* is covered by a committed root" therefore means u ≤ refRoots[r].
\*
\* Durable state:  slotA, slotB (superblocks), walLog, durableRoots
\*                 (chunk store + index contents that have been fsynced).
\* Volatile state: overlay (memtable), chunkBuf (chunk writes not yet
\*                 fsynced), pendingRoot (flushed roots awaiting commit),
\*                 pendingSB, commitPhase.
\* A crash erases all volatile state.
\*
\* Modeling decisions:
\*   - Write collapses memtable-write + WAL-append + WAL-fsync (rev1§4.3
\*     step 2): only acknowledged writes are modeled. A torn WAL tail
\*     holds only unacknowledged records and is discarded at recovery —
\*     no invariant concerns it.
\*   - Commit is two atomic phases. CommitPrepare = fsync barrier 1 +
\*     building the new superblock; CommitFinish = superblock write +
\*     fsync barrier 2. A crash between them resolves the slot being
\*     written nondeterministically to {unchanged, fully written, torn};
\*     torn = checksum-invalid, discarded by recovery (rev1§4.5).
\*   - Crash without barrier 1 may leave some chunkBuf writes durable
\*     (page-cache leakage). Not modeled: such chunks are referenced by
\*     no superblock, invisible to recovery, reclaimed by GC (rev1§4.5) —
\*     they cannot affect any invariant below.
\*   - Flush picks the newest overlay version (last-write-wins). Flush
\*     and commit are mutually exclusive (one storage thread owns the
\*     commit critical section); writes proceed concurrently.
\*
\* Headline invariant (spec rev1§6): after any crash, recovered state =
\* committed roots + replay of all WAL records not covered by the
\* committed head — checked as AckedWritesRecoverable over durable state
\* at every step, partial flushes included (Refs ≥ 2 in the model).

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Refs,       \* set of ref identifiers, e.g. {"r1", "r2"}
    MaxWrites,  \* writes per ref — bounds the state space
    NULL        \* sentinel (model value)

VARIABLES
    \* -- durable --
    slotA, slotB,   \* superblock slots
    walLog,         \* sequence of acked records <<ref, version>>
    durableRoots,   \* set of <<ref, version>> roots fsynced into the chunk store
    \* -- volatile --
    overlay,        \* Refs -> SUBSET Nat — unflushed acked versions
    chunkBuf,       \* set of <<ref, version>> roots written but not fsynced
    pendingRoot,    \* Refs -> Nat — flushed root awaiting commit (0 = none)
    pendingSB,      \* superblock under construction, or NULL
    commitPhase,    \* "idle" | "prepared"
    \* -- control / ghost --
    crashed,        \* TRUE between Crash and Recover
    writeCtr        \* Refs -> Nat — monotone version source (ghost)

vars == <<slotA, slotB, walLog, durableRoots, overlay, chunkBuf,
          pendingRoot, pendingSB, commitPhase, crashed, writeCtr>>

Superblock(gen, refRoots, walHead, valid) ==
    [generation |-> gen, refRoots |-> refRoots,
     walHead |-> walHead, valid |-> valid]

ZeroRoots == [r \in Refs |-> 0]
TornSlot  == Superblock(0, ZeroRoots, 0, FALSE)

Max(S) == CHOOSE x \in S : \A y \in S : y <= x

\* The live superblock: valid slot with the higher generation (rev1§4.5).
LiveSlot ==
    IF slotA.valid /\ slotB.valid
    THEN IF slotA.generation >= slotB.generation THEN slotA ELSE slotB
    ELSE IF slotA.valid THEN slotA ELSE slotB

OlderIsA ==
    IF slotA.valid /\ slotB.valid
    THEN slotA.generation < slotB.generation
    ELSE ~slotA.valid

Init ==
    /\ slotA = Superblock(0, ZeroRoots, 0, TRUE)
    /\ slotB = TornSlot
    /\ walLog = << >>
    /\ durableRoots = {}
    /\ overlay = [r \in Refs |-> {}]
    /\ chunkBuf = {}
    /\ pendingRoot = ZeroRoots
    /\ pendingSB = NULL
    /\ commitPhase = "idle"
    /\ crashed = FALSE
    /\ writeCtr = ZeroRoots

\* --- Actions -----------------------------------------------------------

\* Acknowledged write: memtable + WAL record fsynced before ack.
Write(r) ==
    /\ ~crashed
    /\ writeCtr[r] < MaxWrites
    /\ LET v == writeCtr[r] + 1 IN
        /\ writeCtr' = [writeCtr EXCEPT ![r] = v]
        /\ overlay'  = [overlay EXCEPT ![r] = @ \cup {v}]
        /\ walLog'   = Append(walLog, <<r, v>>)
    /\ UNCHANGED <<slotA, slotB, durableRoots, chunkBuf, pendingRoot,
                   pendingSB, commitPhase, crashed>>

\* Flush a ref's frozen overlay into (not yet durable) tree + chunks.
\* Nothing on disk references the result yet (rev1§4.3 step 3).
Flush(r) ==
    /\ ~crashed
    /\ commitPhase = "idle"
    /\ overlay[r] /= {}
    /\ LET v == Max(overlay[r]) IN
        /\ chunkBuf'    = chunkBuf \cup {<<r, v>>}
        /\ pendingRoot' = [pendingRoot EXCEPT ![r] = v]
        /\ overlay'     = [overlay EXCEPT ![r] = {}]
    /\ UNCHANGED <<slotA, slotB, walLog, durableRoots, pendingSB,
                   commitPhase, crashed, writeCtr>>

\* Barrier 1: fsync chunk store, then build the new superblock.
\* The new walHead is the longest contiguous prefix of records whose
\* effects are flushed — the tail stays pinned by the oldest unflushed
\* record (rev1§4.3 step 4, rev1§4.4). A commit may carry any subset of refs
\* (partial flush): unflushed refs keep their previous committed roots.
CommitPrepare ==
    /\ ~crashed
    /\ commitPhase = "idle"
    /\ \E r \in Refs : pendingRoot[r] /= 0
    /\ durableRoots' = durableRoots \cup chunkBuf
    /\ chunkBuf' = {}
    /\ LET newRoots == [r \in Refs |->
                          IF pendingRoot[r] /= 0 THEN pendingRoot[r]
                          ELSE LiveSlot.refRoots[r]]
           Covered(i) == walLog[i][2] <= newRoots[walLog[i][1]]
           newHead == CHOOSE h \in 0..Len(walLog) :
                          /\ \A i \in 1..h : Covered(i)
                          /\ h = Len(walLog) \/ ~Covered(h + 1)
       IN pendingSB' = Superblock(LiveSlot.generation + 1, newRoots,
                                  newHead, TRUE)
    /\ commitPhase' = "prepared"
    /\ UNCHANGED <<slotA, slotB, walLog, overlay, pendingRoot, crashed,
                   writeCtr>>

\* Write the new superblock to the OLDER slot, then barrier 2 (fsync).
\* Only after this is the commit real (rev1§4.3 step 4).
CommitFinish ==
    /\ ~crashed
    /\ commitPhase = "prepared"
    /\ IF OlderIsA
       THEN slotA' = pendingSB /\ slotB' = slotB
       ELSE slotB' = pendingSB /\ slotA' = slotA
    /\ pendingRoot' = ZeroRoots
    /\ pendingSB' = NULL
    /\ commitPhase' = "idle"
    /\ UNCHANGED <<walLog, durableRoots, overlay, chunkBuf, crashed,
                   writeCtr>>

\* Crash at any point. Volatile state is lost. If the superblock write
\* window was open, the older slot lands in one of three states: the
\* write never reached disk, fully reached disk (then barrier 2 was
\* merely redundant), or tore — a torn write can only damage the slot
\* being written, never the other one (rev1§4.5).
Crash ==
    /\ ~crashed
    /\ crashed' = TRUE
    /\ IF commitPhase = "prepared"
       THEN \E outcome \in {"old", "new", "torn"} :
              IF OlderIsA
              THEN /\ slotA' = CASE outcome = "old"  -> slotA
                                 [] outcome = "new"  -> pendingSB
                                 [] outcome = "torn" -> TornSlot
                   /\ slotB' = slotB
              ELSE /\ slotB' = CASE outcome = "old"  -> slotB
                                 [] outcome = "new"  -> pendingSB
                                 [] outcome = "torn" -> TornSlot
                   /\ slotA' = slotA
       ELSE UNCHANGED <<slotA, slotB>>
    /\ overlay' = [r \in Refs |-> {}]
    /\ chunkBuf' = {}
    /\ pendingRoot' = ZeroRoots
    /\ pendingSB' = NULL
    /\ commitPhase' = "idle"
    /\ UNCHANGED <<walLog, durableRoots, writeCtr>>

\* Recovery (rev1§4.5): the live slot defines reality; rebuild each ref's
\* overlay by replaying WAL records past the committed head. Records past
\* the head already covered by the committed root replay idempotently —
\* the version filter models that.
Recover ==
    /\ crashed
    /\ LET L == LiveSlot IN
        overlay' = [r \in Refs |->
                      {walLog[i][2] :
                        i \in {j \in (L.walHead + 1)..Len(walLog) :
                                 /\ walLog[j][1] = r
                                 /\ walLog[j][2] > L.refRoots[r]}}]
    /\ crashed' = FALSE
    /\ UNCHANGED <<slotA, slotB, walLog, durableRoots, chunkBuf,
                   pendingRoot, pendingSB, commitPhase, writeCtr>>

Next ==
    \/ \E r \in Refs : Write(r)
    \/ \E r \in Refs : Flush(r)
    \/ CommitPrepare
    \/ CommitFinish
    \/ Crash
    \/ Recover

Spec == Init /\ [][Next]_vars

\* --- Invariants --------------------------------------------------------

SBOK(s) ==
    /\ s.generation \in Nat
    /\ s.walHead \in 0..Len(walLog)
    /\ s.valid \in BOOLEAN
    /\ \A r \in Refs : s.refRoots[r] \in 0..writeCtr[r]

TypeOK ==
    /\ SBOK(slotA) /\ SBOK(slotB)
    /\ \A i \in 1..Len(walLog) :
         walLog[i][1] \in Refs /\ walLog[i][2] \in 1..MaxWrites
    /\ \A r \in Refs : overlay[r] \subseteq 1..writeCtr[r]
    /\ \A r \in Refs : pendingRoot[r] \in 0..writeCtr[r]
    /\ commitPhase \in {"idle", "prepared"}
    /\ (commitPhase = "prepared") <=> (pendingSB /= NULL)

\* rev1§4.5: a torn superblock write can only damage the slot being written;
\* a complete older commit always survives.
AtLeastOneValidSlot == slotA.valid \/ slotB.valid

\* LiveSlot must be deterministic: two valid slots never share a generation.
GenerationsDistinct ==
    (slotA.valid /\ slotB.valid) => slotA.generation /= slotB.generation

\* Barrier 1 (rev1§4.3): no superblock — not even a stale-but-valid one —
\* may reference chunks that are not durable.
CommittedRootsDurable ==
    \A s \in {slotA, slotB} :
        s.valid => \A r \in Refs :
            s.refRoots[r] /= 0 => <<r, s.refRoots[r]>> \in durableRoots

\* The headline recovery invariant (rev1§6): every acknowledged write is
\* recoverable from durable state alone — its effects are in the live
\* slot's committed root, or its WAL record lies past the committed head
\* and will be replayed. Holds at every step, so it holds after any crash.
AckedWritesRecoverable ==
    \A r \in Refs : \A v \in 1..writeCtr[r] :
        \/ v <= LiveSlot.refRoots[r]
        \/ \E i \in (LiveSlot.walHead + 1)..Len(walLog) :
              walLog[i] = <<r, v>>

====
