---- MODULE CapRevocation ----
\* Kernel capability revocation model for Eunomia OS (spec §2.2, §3.4, §6).
\*
\* Models the capability derivation tree (CDT), per-process cspaces,
\* channel queues holding in-flight caps, copy/send/receive/revoke/retype.
\*
\* Key properties checked:
\*   - MoveSemantics: at every instant a live cap has exactly one owner —
\*     a process cspace or a queue slot, never two (spec §3.4).
\*   - LiveParent: a live cap's CDT parent is live. Together with
\*     DeadNowhere this is "revoke destroys all descendants" in invariant
\*     form: no descendant can survive its ancestor's revocation anywhere,
\*     channel queues included — unconditionally, no "except messages in
\*     flight" caveat (spec §2.2).
\*   - DeadNowhere: a deleted cap appears in no cspace and no queue slot.
\*   - RevokedDead: ghost check — a revoked slot stays dead until reused
\*     by a fresh Copy.
\*
\* Modeling notes:
\*   - Revoke is atomic here. The kernel walk is preemptible/restartable;
\*     its postcondition (no live descendants on completion) is what this
\*     model checks. The deletion order constraint the implementation must
\*     respect (delete leaf-first / DFS post-order, so LiveParent holds at
\*     every preemption point) is recorded here as the obligation.
\*   - Receivers tolerate sparse messages: revoke deletes caps out of
\*     queued messages in place, so a message can arrive with fewer caps
\*     than were sent — the "null cap slots" of §3.4.
\*   - Retype models only its authority precondition: it requires that the
\*     cap has no live descendants (exclusivity proven by revoke, §2.2).

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    CapIds,     \* finite set of cap slot IDs
    Procs,      \* set of process IDs
    Channels,   \* set of channel IDs
    QueueDepth, \* per-channel queue capacity (donated at creation, §3.2)
    NULL        \* null/empty sentinel (model value)

MaxCapsPerMsg == 4  \* spec §3.1: 4 cap slots per message

VARIABLES
    live,       \* SUBSET CapIds — allocated cap slots
    parent,     \* CapIds -> CapIds \cup {NULL}  (CDT parent pointer)
    cspaces,    \* Procs -> SUBSET CapIds  (caps owned by each process)
    queues,     \* Channels -> Seq(SUBSET CapIds)  (in-flight cap sets)
    revoked     \* SUBSET CapIds — ghost: revoked ids not yet reused

vars == <<live, parent, cspaces, queues, revoked>>

\* All descendants of cap in the CDT (recursive). Dead caps have parent
\* NULL, so the walk only ever finds live caps.
RECURSIVE Descendants(_)
Descendants(cap) ==
    LET children == {c \in CapIds : parent[c] = cap}
    IN  children \cup UNION {Descendants(c) : c \in children}

InitCap  == CHOOSE c \in CapIds : TRUE
InitProc == CHOOSE p \in Procs : TRUE

Init ==
    /\ live     = {InitCap}
    /\ parent   = [c \in CapIds |-> NULL]
    /\ cspaces  = [p \in Procs |-> IF p = InitProc THEN {InitCap} ELSE {}]
    /\ queues   = [ch \in Channels |-> << >>]
    /\ revoked  = {}

\* --- Actions -----------------------------------------------------------

\* Copy/mint: process p derives a child of src into free slot dst.
\* Reusing a previously revoked slot id makes a fresh cap, so the ghost
\* forgets it.
Copy(p, src, dst) ==
    /\ src \in cspaces[p]
    /\ dst \notin live
    /\ live'    = live \cup {dst}
    /\ parent'  = [parent EXCEPT ![dst] = src]
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup {dst}]
    /\ revoked' = revoked \ {dst}
    /\ UNCHANGED queues

\* Send: caps move from the sender's cspace into a queue slot (§3.4).
\* Non-blocking; disabled (FULL) when the queue is at capacity (§3.3).
Send(p, ch, cs) ==
    /\ cs /= {}
    /\ cs \subseteq cspaces[p]
    /\ Cardinality(cs) <= MaxCapsPerMsg
    /\ Len(queues[ch]) < QueueDepth
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \ cs]
    /\ queues'  = [queues EXCEPT ![ch] = Append(@, cs)]
    /\ UNCHANGED <<live, parent, revoked>>

\* Receive: head message's surviving caps land in the receiver's cspace.
\* (Cspace-slot exhaustion makes receive fail with the message left
\* queued, §3.3 — equivalent to this action simply not being taken.)
Receive(p, ch) ==
    /\ queues[ch] /= << >>
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup Head(queues[ch])]
    /\ queues'  = [queues EXCEPT ![ch] = Tail(@)]
    /\ UNCHANGED <<live, parent, revoked>>

\* Revoke: delete every CDT descendant of c — from cspaces AND from
\* queued messages. c itself stays live (seL4 semantics: revoke empties
\* the subtree below the cap, establishing exclusivity for retype).
Revoke(c) ==
    /\ c \in live
    /\ LET dead == Descendants(c) IN
        /\ dead /= {}   \* no-op revokes add nothing to the state space
        /\ live'    = live \ dead
        /\ parent'  = [x \in CapIds |-> IF x \in dead THEN NULL ELSE parent[x]]
        /\ cspaces' = [p \in Procs |-> cspaces[p] \ dead]
        /\ queues'  = [ch \in Channels |->
                          [i \in 1..Len(queues[ch]) |-> queues[ch][i] \ dead]]
        /\ revoked' = revoked \cup dead

\* Retype: consume an exclusive cap (e.g. untyped -> kernel object).
\* Sound only when no derived caps exist anywhere — the guard the kernel
\* establishes by running revoke first (§2.2).
Retype(p, c) ==
    /\ c \in cspaces[p]
    /\ Descendants(c) = {}
    /\ live'    = live \ {c}
    /\ parent'  = [parent EXCEPT ![c] = NULL]
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \ {c}]
    /\ revoked' = revoked \cup {c}
    /\ UNCHANGED queues

Next ==
    \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
    \/ \E p \in Procs, ch \in Channels, cs \in (SUBSET CapIds) : Send(p, ch, cs)
    \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
    \/ \E c \in CapIds : Revoke(c)
    \/ \E p \in Procs, c \in CapIds : Retype(p, c)

Spec == Init /\ [][Next]_vars

\* --- Invariants --------------------------------------------------------

TypeOK ==
    /\ live \subseteq CapIds
    /\ \A c \in CapIds : parent[c] \in CapIds \cup {NULL}
    /\ \A p \in Procs  : cspaces[p] \subseteq CapIds
    /\ \A ch \in Channels :
        /\ Len(queues[ch]) <= QueueDepth
        /\ \A i \in 1..Len(queues[ch]) : queues[ch][i] \subseteq CapIds
    /\ revoked \subseteq CapIds

\* Where a cap currently resides.
ProcPlaces(c)  == {p \in Procs : c \in cspaces[p]}
QueuePlaces(c) == {<<ch, i>> \in Channels \X (1..QueueDepth) :
                      i <= Len(queues[ch]) /\ c \in queues[ch][i]}

\* §3.4: exactly one owner at every instant — sender, queue slot, or
\* receiver, never two.
MoveSemantics ==
    \A c \in live :
        Cardinality(ProcPlaces(c)) + Cardinality(QueuePlaces(c)) = 1

\* A deleted cap exists nowhere — cspaces and queues both purged.
DeadNowhere ==
    \A c \in CapIds \ live :
        ProcPlaces(c) = {} /\ QueuePlaces(c) = {}

\* No descendant survives its ancestor's deletion: revoke is complete,
\* through queues, unconditionally.
LiveParent ==
    \A c \in live : parent[c] = NULL \/ parent[c] \in live

\* Ghost: revoked slots stay dead until explicitly reused.
RevokedDead == revoked \cap live = {}

====
