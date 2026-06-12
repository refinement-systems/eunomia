---- MODULE CapRevocation ----
\* Kernel capability revocation model for Eunomia OS (spec §2.2, §3.4,
\* §5.1, §6).
\*
\* Models the capability derivation tree (CDT), per-process cspaces,
\* channel queues holding in-flight caps, TCB on-exit/on-fault binding
\* slots holding notification caps, copy/send/receive/revoke/retype/
\* bind/thread-death.
\*
\* Key properties checked:
\*   - MoveSemantics: at every instant a live cap has exactly one owner —
\*     a process cspace, a queue slot, or a TCB binding slot, never two
\*     (spec §3.4).
\*   - LiveParent: a live cap's CDT parent is live. Together with
\*     DeadNowhere this is "revoke destroys all descendants" in invariant
\*     form: no descendant can survive its ancestor's revocation anywhere,
\*     channel queues and TCB binding slots included — unconditionally,
\*     no "except messages in flight" caveat (spec §2.2).
\*   - DeadNowhere: a deleted cap appears in no cspace, no queue slot,
\*     and no binding slot.
\*   - FireSafe: a non-NULL binding slot always names a live cap. This is
\*     the §5.1 firing obligation in invariant form: revoking the
\*     notification's lineage racing thread-death must either leave a
\*     live cap for the firing to signal through (the cap holds a ref, so
\*     its object is live) or have cleared the slot (signaling nothing is
\*     a no-op) — never a freed object. It must hold at every preemption
\*     point of the future preemptible revoke walk, which is exactly why
\*     the slots are CDT-visible rather than refcounted raw pointers.
\*   - RevokedDead: ghost check — a revoked slot stays dead until reused
\*     by a fresh Copy.
\*   - ReportMonotone (action property): the terminal report transitions
\*     at most once, running -> exited | faulted (§5.1).
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
\*   - Bind moves the cap out of the binder's cspace into the TCB slot
\*     (§3.4 move semantics; the kernel caller duplicates first to keep
\*     access). Kernel rebind = delete-the-displaced-cap + bind; single-
\*     cap delete (children re-parent one level up) preserves LiveParent
\*     by construction and is not modeled, matching channel destruction
\*     (which deletes queued caps the same way) being unmodeled.
\*   - Thread destruction deletes binding caps with ordinary CDT cleanup
\*     and produces no report and no firing — destruction is the parent
\*     acting, not the thread dying (§5.1). Container teardown, like
\*     channel destruction, is not modeled.

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    CapIds,     \* finite set of cap slot IDs
    Procs,      \* set of process IDs
    Channels,   \* set of channel IDs
    Threads,    \* set of TCB IDs (each carrying exit/fault binding slots)
    QueueDepth, \* per-channel queue capacity (donated at creation, §3.2)
    NULL        \* null/empty sentinel (model value)

MaxCapsPerMsg == 4  \* spec §3.1: 4 cap slots per message

BindKinds == {"exit", "fault"}  \* the two fixed TCB binding slots (§5.1)

VARIABLES
    live,       \* SUBSET CapIds — allocated cap slots
    parent,     \* CapIds -> CapIds \cup {NULL}  (CDT parent pointer)
    cspaces,    \* Procs -> SUBSET CapIds  (caps owned by each process)
    queues,     \* Channels -> Seq(SUBSET CapIds)  (in-flight cap sets)
    bindings,   \* Threads -> [BindKinds -> CapIds \cup {NULL}]
    treport,    \* Threads -> {"running", "exited", "faulted"}  (§5.1)
    revoked     \* SUBSET CapIds — ghost: revoked ids not yet reused

vars == <<live, parent, cspaces, queues, bindings, treport, revoked>>

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
    /\ bindings = [t \in Threads |-> [k \in BindKinds |-> NULL]]
    /\ treport  = [t \in Threads |-> "running"]
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
    /\ UNCHANGED <<queues, bindings, treport>>

\* Send: caps move from the sender's cspace into a queue slot (§3.4).
\* Non-blocking; disabled (FULL) when the queue is at capacity (§3.3).
Send(p, ch, cs) ==
    /\ cs /= {}
    /\ cs \subseteq cspaces[p]
    /\ Cardinality(cs) <= MaxCapsPerMsg
    /\ Len(queues[ch]) < QueueDepth
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \ cs]
    /\ queues'  = [queues EXCEPT ![ch] = Append(@, cs)]
    /\ UNCHANGED <<live, parent, bindings, treport, revoked>>

\* Receive: head message's surviving caps land in the receiver's cspace.
\* (Cspace-slot exhaustion makes receive fail with the message left
\* queued, §3.3 — equivalent to this action simply not being taken.)
Receive(p, ch) ==
    /\ queues[ch] /= << >>
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup Head(queues[ch])]
    /\ queues'  = [queues EXCEPT ![ch] = Tail(@)]
    /\ UNCHANGED <<live, parent, bindings, treport, revoked>>

\* Bind: configure a TCB binding slot (§5.1) — the cap moves from the
\* binder's cspace into the slot, exactly as Send moves caps into queue
\* slots. Binding is legal regardless of the thread's report state (a
\* late binding simply never fires).
Bind(p, t, k, c) ==
    /\ c \in cspaces[p]
    /\ bindings[t][k] = NULL
    /\ bindings' = [bindings EXCEPT ![t][k] = c]
    /\ cspaces'  = [cspaces EXCEPT ![p] = @ \ {c}]
    /\ UNCHANGED <<live, parent, queues, treport, revoked>>

\* Thread death (§5.1): the terminal record transitions exactly once,
\* and the kernel fires the matching binding slot at this instant — it
\* reads bindings[t][k] and signals through the cap if one is present.
\* The firing itself moves no caps; its safety is the FireSafe invariant
\* holding in the pre-state of this action.
ThreadExit(t) ==
    /\ treport[t] = "running"
    /\ treport' = [treport EXCEPT ![t] = "exited"]
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, revoked>>

ThreadFault(t) ==
    /\ treport[t] = "running"
    /\ treport' = [treport EXCEPT ![t] = "faulted"]
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, revoked>>

\* Revoke: delete every CDT descendant of c — from cspaces AND from
\* queued messages AND from TCB binding slots. c itself stays live (seL4
\* semantics: revoke empties the subtree below the cap, establishing
\* exclusivity for retype).
Revoke(c) ==
    /\ c \in live
    /\ LET dead == Descendants(c) IN
        /\ dead /= {}   \* no-op revokes add nothing to the state space
        /\ live'    = live \ dead
        /\ parent'  = [x \in CapIds |-> IF x \in dead THEN NULL ELSE parent[x]]
        /\ cspaces' = [p \in Procs |-> cspaces[p] \ dead]
        /\ queues'  = [ch \in Channels |->
                          [i \in 1..Len(queues[ch]) |-> queues[ch][i] \ dead]]
        /\ bindings' = [t \in Threads |-> [k \in BindKinds |->
                          IF bindings[t][k] \in dead THEN NULL
                          ELSE bindings[t][k]]]
        /\ revoked' = revoked \cup dead
        /\ UNCHANGED treport

\* Retype: consume an exclusive cap (e.g. untyped -> kernel object).
\* Sound only when no derived caps exist anywhere — the guard the kernel
\* establishes by running revoke first (§2.2). Caps parked in binding
\* slots are not in any cspace, so they cannot be retyped; as CDT
\* residents they still block an ancestor's retype via Descendants.
Retype(p, c) ==
    /\ c \in cspaces[p]
    /\ Descendants(c) = {}
    /\ live'    = live \ {c}
    /\ parent'  = [parent EXCEPT ![c] = NULL]
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \ {c}]
    /\ revoked' = revoked \cup {c}
    /\ UNCHANGED <<queues, bindings, treport>>

Next ==
    \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
    \/ \E p \in Procs, ch \in Channels, cs \in (SUBSET CapIds) : Send(p, ch, cs)
    \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
    \/ \E p \in Procs, t \in Threads, k \in BindKinds, c \in CapIds : Bind(p, t, k, c)
    \/ \E t \in Threads : ThreadExit(t)
    \/ \E t \in Threads : ThreadFault(t)
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
    /\ bindings \in [Threads -> [BindKinds -> CapIds \cup {NULL}]]
    /\ treport \in [Threads -> {"running", "exited", "faulted"}]
    /\ revoked \subseteq CapIds

\* Where a cap currently resides.
ProcPlaces(c)  == {p \in Procs : c \in cspaces[p]}
QueuePlaces(c) == {<<ch, i>> \in Channels \X (1..QueueDepth) :
                      i <= Len(queues[ch]) /\ c \in queues[ch][i]}
BindPlaces(c)  == {<<t, k>> \in Threads \X BindKinds : bindings[t][k] = c}

\* §3.4: exactly one owner at every instant — a cspace, a queue slot, or
\* a TCB binding slot, never two.
MoveSemantics ==
    \A c \in live :
        Cardinality(ProcPlaces(c)) + Cardinality(QueuePlaces(c))
            + Cardinality(BindPlaces(c)) = 1

\* A deleted cap exists nowhere — cspaces, queues, and binding slots all
\* purged.
DeadNowhere ==
    \A c \in CapIds \ live :
        ProcPlaces(c) = {} /\ QueuePlaces(c) = {} /\ BindPlaces(c) = {}

\* No descendant survives its ancestor's deletion: revoke is complete,
\* through queues and binding slots, unconditionally.
LiveParent ==
    \A c \in live : parent[c] = NULL \/ parent[c] \in live

\* §5.1's firing obligation: a configured binding slot names a live cap
\* — so a thread-death firing signals a live object (the cap's ref keeps
\* the notification alive) or skips a cleared slot, never touches a
\* freed one. Implied by DeadNowhere; named because it is the property
\* the kernel's preemptible revoke walk must preserve at every step.
FireSafe ==
    \A t \in Threads, k \in BindKinds :
        bindings[t][k] = NULL \/ bindings[t][k] \in live

\* Ghost: revoked slots stay dead until explicitly reused.
RevokedDead == revoked \cap live = {}

\* §5.1: at most one terminal report per thread, ever — the record never
\* leaves a terminal state.
ReportMonotone ==
    [][\A t \in Threads :
        treport[t] /= "running" => treport'[t] = treport[t]]_vars

====
