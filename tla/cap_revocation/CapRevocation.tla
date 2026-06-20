---- MODULE CapRevocation ----
\* Kernel capability revocation model for Eunomia OS (spec rev1§2.2, rev1§3.4,
\* rev1§5.1, rev1§6).
\*
\* Models the capability derivation tree (CDT), per-process cspaces,
\* channel queues holding in-flight caps, TCB on-exit/on-fault binding
\* slots holding notification caps, copy/send/receive/revoke/retype/
\* bind/thread-death.
\*
\* Two specifications share this module and its variables. Spec is the CDT
\* revocation model (the bulk below). TSpec is the rev1§3.3 channel
\* whole-object teardown model — peer-closed bindings and their firing —
\* checked by CapRevocation_Teardown.cfg; see its own header further down.
\* Each spec keeps the other's variables constant, so they do not multiply
\* each other's state space.
\*
\* Key properties checked:
\*   - MoveSemantics: at every instant a live cap has exactly one owner —
\*     a process cspace, a queue slot, or a TCB binding slot, never two
\*     (spec rev1§3.4).
\*   - LiveParent: a live cap's CDT parent is live. Together with
\*     DeadNowhere this is "revoke destroys all descendants" in invariant
\*     form: no descendant can survive its ancestor's revocation anywhere,
\*     channel queues and TCB binding slots included — unconditionally,
\*     no "except messages in flight" caveat (spec rev1§2.2).
\*   - DeadNowhere: a deleted cap appears in no cspace, no queue slot,
\*     and no binding slot.
\*   - FireSafe: a non-NULL binding slot always names a live cap. This is
\*     the rev1§5.1 firing obligation in invariant form: revoking the
\*     notification's lineage racing thread-death must either leave a
\*     live cap for the firing to signal through (the cap holds a ref, so
\*     its object is live) or have cleared the slot (signaling nothing is
\*     a no-op) — never a freed object. It must hold at every preemption
\*     point of the future preemptible revoke walk, which is exactly why
\*     the slots are CDT-visible rather than refcounted raw pointers.
\*   - RevokedDead: ghost check — a revoked slot stays dead until reused
\*     by a fresh Copy.
\*   - ReportMonotone (action property): the terminal report transitions
\*     at most once, running -> exited | faulted (rev1§5.1).
\*
\* TSpec properties (rev1§3.3 channel teardown):
\*   - ChannelFireSafe: every peer-closed binding on a live channel names a
\*     live notification — the teardown firing signals a live object, never
\*     a freed one, even when the notification's whole cap lineage was
\*     revoked (the channel's hold keeps it alive).
\*   - RefCountSound: a notification is alive iff a cap or a channel hold
\*     references it — the refcount discipline the kernel maintains.
\*   - ReclaimedReleased: a reclaimed channel holds no notification.
\*
\* Modeling notes:
\*   - Revoke is atomic here. The kernel walk is preemptible/restartable;
\*     its postcondition (no live descendants on completion) is what this
\*     model checks. The deletion order constraint the implementation must
\*     respect (delete leaf-first / DFS post-order, so LiveParent holds at
\*     every preemption point) is recorded here as the obligation.
\*   - Receivers tolerate sparse messages: revoke deletes caps out of
\*     queued messages in place, so a message can arrive with fewer caps
\*     than were sent — the "null cap slots" of rev1§3.4.
\*   - Retype models only its authority precondition: it requires that the
\*     cap has no live descendants (exclusivity proven by revoke, rev1§2.2).
\*   - Bind moves the cap out of the binder's cspace into the TCB slot
\*     (rev1§3.4 move semantics; the kernel caller duplicates first to keep
\*     access). Kernel rebind = delete-the-displaced-cap + bind; single-
\*     cap delete (children re-parent one level up) preserves LiveParent
\*     by construction and is not modeled. Channel destruction's queued-cap
\*     cleanup is the same shape and likewise unmodeled here; its NEW
\*     content — peer-closed firing under whole-object teardown — is the
\*     separate TSpec at the foot of this file (rev1§3.3).
\*   - Thread destruction deletes binding caps with ordinary CDT cleanup
\*     and produces no report and no firing — destruction is the parent
\*     acting, not the thread dying (rev1§5.1).

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    CapIds,     \* finite set of cap slot IDs
    Procs,      \* set of process IDs
    Channels,   \* set of channel IDs
    Threads,    \* set of TCB IDs (each carrying exit/fault binding slots)
    Notifs,     \* notification object IDs (TSpec; rev1§3.3 channel teardown)
    QueueDepth, \* per-channel queue capacity (donated at creation, rev1§3.2)
    NULL        \* null/empty sentinel (model value)

MaxCapsPerMsg == 4  \* spec rev1§3.1: 4 cap slots per message

BindKinds == {"exit", "fault"}  \* the two fixed TCB binding slots (rev1§5.1)

Ends     == {0, 1}  \* the two channel endpoints (TSpec)
MaxNCaps == 2       \* bound on outstanding caps per notification (TSpec)

VARIABLES
    live,       \* SUBSET CapIds — allocated cap slots
    parent,     \* CapIds -> CapIds \cup {NULL}  (CDT parent pointer)
    cspaces,    \* Procs -> SUBSET CapIds  (caps owned by each process)
    queues,     \* Channels -> Seq(SUBSET CapIds)  (in-flight cap sets)
    bindings,   \* Threads -> [BindKinds -> CapIds \cup {NULL}]
    treport,    \* Threads -> {"running", "exited", "faulted"}  (rev1§5.1)
    revoked,    \* SUBSET CapIds — ghost: revoked ids not yet reused
    \* --- TSpec only (channel teardown, rev1§3.3); constant under Spec -------
    nlive,      \* SUBSET Notifs — alive notification objects
    ncaps,      \* Notifs -> 0..MaxNCaps  (outstanding caps per notif)
    pcbind,     \* Channels -> [Ends -> Notifs \cup {NULL}]  (peer-closed)
    eopen       \* Channels -> [Ends -> BOOLEAN]  (endpoint has a live cap)

\* The revocation half (Spec) and the channel-teardown half (TSpec) carry
\* disjoint variables; each half holds the other's constant, so neither
\* multiplies the other's state space.
crVars == <<live, parent, cspaces, queues, bindings, treport, revoked>>
tdVars == <<nlive, ncaps, pcbind, eopen>>
vars   == <<live, parent, cspaces, queues, bindings, treport, revoked,
            nlive, ncaps, pcbind, eopen>>

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
    \* TSpec variables — constant under Spec, evolving under TSpec.
    /\ nlive    = {}
    /\ ncaps    = [n \in Notifs |-> 0]
    /\ pcbind   = [ch \in Channels |-> [e \in Ends |-> NULL]]
    /\ eopen    = [ch \in Channels |-> [e \in Ends |-> TRUE]]

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

\* Send: caps move from the sender's cspace into a queue slot (rev1§3.4).
\* Non-blocking; disabled (FULL) when the queue is at capacity (rev1§3.3).
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
\* queued, rev1§3.3 — equivalent to this action simply not being taken.)
Receive(p, ch) ==
    /\ queues[ch] /= << >>
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup Head(queues[ch])]
    /\ queues'  = [queues EXCEPT ![ch] = Tail(@)]
    /\ UNCHANGED <<live, parent, bindings, treport, revoked>>

\* Bind: configure a TCB binding slot (rev1§5.1) — the cap moves from the
\* binder's cspace into the slot, exactly as Send moves caps into queue
\* slots. Binding is legal regardless of the thread's report state (a
\* late binding simply never fires).
Bind(p, t, k, c) ==
    /\ c \in cspaces[p]
    /\ bindings[t][k] = NULL
    /\ bindings' = [bindings EXCEPT ![t][k] = c]
    /\ cspaces'  = [cspaces EXCEPT ![p] = @ \ {c}]
    /\ UNCHANGED <<live, parent, queues, treport, revoked>>

\* Thread death (rev1§5.1): the terminal record transitions exactly once,
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
\* establishes by running revoke first (rev1§2.2). Caps parked in binding
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
    /\ \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
       \/ \E p \in Procs, ch \in Channels, cs \in (SUBSET CapIds) : Send(p, ch, cs)
       \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
       \/ \E p \in Procs, t \in Threads, k \in BindKinds, c \in CapIds : Bind(p, t, k, c)
       \/ \E t \in Threads : ThreadExit(t)
       \/ \E t \in Threads : ThreadFault(t)
       \/ \E c \in CapIds : Revoke(c)
       \/ \E p \in Procs, c \in CapIds : Retype(p, c)
    /\ UNCHANGED tdVars

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

\* rev1§3.4: exactly one owner at every instant — a cspace, a queue slot, or
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

\* rev1§5.1's firing obligation: a configured binding slot names a live cap
\* — so a thread-death firing signals a live object (the cap's ref keeps
\* the notification alive) or skips a cleared slot, never touches a
\* freed one. Implied by DeadNowhere; named because it is the property
\* the kernel's preemptible revoke walk must preserve at every step.
FireSafe ==
    \A t \in Threads, k \in BindKinds :
        bindings[t][k] = NULL \/ bindings[t][k] \in live

\* Ghost: revoked slots stay dead until explicitly reused.
RevokedDead == revoked \cap live = {}

\* rev1§5.1: at most one terminal report per thread, ever — the record never
\* leaves a terminal state.
ReportMonotone ==
    [][\A t \in Threads :
        treport[t] /= "running" => treport'[t] = treport[t]]_vars

\* =======================================================================
\* Channel whole-object teardown and peer-closed firing (rev1§3.3) — TSpec
\* =======================================================================
\* The rev1§3.3 teardown rule: deleting one endpoint fires the surviving
\* peer's peer-closed binding, and destroying the whole object at once —
\* its backing untyped revoked — fires EVERY endpoint's binding before
\* reclamation, each firing naming a LIVE notification, never a freed one.
\*
\* Modeled as a self-contained second spec (TSpec, checked by
\* CapRevocation_Teardown.cfg) because channel peer-closed bindings use a
\* DIFFERENT lifetime mechanism than the TCB exit/fault slots above. The
\* TCB slots are CDT-visible: `bind` MOVES the cap in, and revocation sees
\* through the slot (the FireSafe story). Channel `bind` instead bumps the
\* notification OBJECT's refcount and leaves the binder's cap in place, so
\* revocation does NOT see through the slot: the notification "outlives the
\* channel if separately funded" (rev1§3.3), and — the property that makes the
\* firing safe — a notification whose entire cap lineage is revoked stays
\* alive as long as a channel still holds it. That is a refcount
\* discipline, modeled here with explicit notification objects (`nlive`),
\* their cap counts (`ncaps`), and the channel holds (`pcbind`). Each half
\* holds the other's variables constant, so TSpec costs the revocation
\* proof nothing and vice versa.

\* Channel holds on a notification = the non-NULL peer-closed slots naming
\* it; a hold keeps the object alive independent of any cap (the refcount).
Holds(n) ==
    Cardinality({ce \in Channels \X Ends : pcbind[ce[1]][ce[2]] = n})
HoldsExcept(n, ch) ==
    Cardinality({ce \in Channels \X Ends :
                    ce[1] /= ch /\ pcbind[ce[1]][ce[2]] = n})
\* A channel is reclaimed once BOTH endpoint caps are gone; until then it
\* is alive and its holds stand.
ChanAlive(ch) == eopen[ch][0] \/ eopen[ch][1]

\* A fresh notification object: one cap, no holds. An id is reusable only
\* once fully dead (no caps, no holds), mirroring revoked-slot reuse above.
NewNotif(n) ==
    /\ n \notin nlive
    /\ ncaps[n] = 0
    /\ Holds(n) = 0
    /\ nlive' = nlive \cup {n}
    /\ ncaps' = [ncaps EXCEPT ![n] = 1]
    /\ UNCHANGED <<pcbind, eopen>>

\* Mint another cap to a live notification — it is "separately funded".
NotifCopy(n) ==
    /\ n \in nlive
    /\ ncaps[n] < MaxNCaps
    /\ ncaps' = [ncaps EXCEPT ![n] = @ + 1]
    /\ UNCHANGED <<nlive, pcbind, eopen>>

\* Delete one cap; the object dies when its last reference (cap or hold) is
\* gone.
NotifDropCap(n) ==
    /\ n \in nlive
    /\ ncaps[n] > 0
    /\ ncaps' = [ncaps EXCEPT ![n] = @ - 1]
    /\ nlive' = IF (ncaps[n] - 1) + Holds(n) = 0 THEN nlive \ {n} ELSE nlive
    /\ UNCHANGED <<pcbind, eopen>>

\* Revoke a notification's WHOLE cap lineage at once (rev1§2.2). The object
\* survives iff a channel still holds it — the refcount-keeps-alive
\* property that makes a teardown firing safe after the holder's caps die.
RevokeNotif(n) ==
    /\ n \in nlive
    /\ ncaps[n] > 0
    /\ ncaps' = [ncaps EXCEPT ![n] = 0]
    /\ nlive' = IF Holds(n) = 0 THEN nlive \ {n} ELSE nlive
    /\ UNCHANGED <<pcbind, eopen>>

\* Configure an endpoint's peer-closed binding (rev1§3.6): the channel takes a
\* hold on a live notification. Bind through a live endpoint cap into a
\* free slot; the binder keeps its own cap (refcount bump, not a move).
ChanBindPC(ch, e, n) ==
    /\ ChanAlive(ch)
    /\ eopen[ch][e]
    /\ pcbind[ch][e] = NULL
    /\ n \in nlive
    /\ pcbind' = [pcbind EXCEPT ![ch][e] = n]
    /\ UNCHANGED <<nlive, ncaps, eopen>>

\* The last cap of endpoint e is deleted (a single close, or one step of a
\* whole-object teardown when the channel's backing untyped is revoked).
\* The kernel fires the OTHER end's peer-closed binding here; by
\* ChannelFireSafe that binding (if set) names a live notification — and a
\* still-open OR already-closed peer's binding stands until reclamation, so
\* whole-object teardown fires both ends. When this close empties the
\* second endpoint the channel is reclaimed: holds release, and a
\* notification kept alive only by those holds dies. The firing reads this
\* step's pre-state; reclamation is its post-state — fire precedes reclaim.
CloseEndpoint(ch, e) ==
    /\ ChanAlive(ch)
    /\ eopen[ch][e]
    /\ eopen' = [eopen EXCEPT ![ch][e] = FALSE]
    /\ IF ~eopen[ch][1 - e]            \* peer already closed → reclaim
       THEN /\ pcbind' = [pcbind EXCEPT ![ch] = [x \in Ends |-> NULL]]
            /\ nlive'  = nlive \ {n \in nlive :
                            ncaps[n] = 0 /\ HoldsExcept(n, ch) = 0}
       ELSE UNCHANGED <<pcbind, nlive>>
    /\ UNCHANGED ncaps

TNext ==
    /\ \/ \E n \in Notifs : NewNotif(n)
       \/ \E n \in Notifs : NotifCopy(n)
       \/ \E n \in Notifs : NotifDropCap(n)
       \/ \E n \in Notifs : RevokeNotif(n)
       \/ \E ch \in Channels, e \in Ends, n \in Notifs : ChanBindPC(ch, e, n)
       \/ \E ch \in Channels, e \in Ends : CloseEndpoint(ch, e)
    /\ UNCHANGED crVars

TSpec == Init /\ [][TNext]_vars

\* --- TSpec invariants --------------------------------------------------

TTypeOK ==
    /\ nlive \subseteq Notifs
    /\ ncaps  \in [Notifs -> 0..MaxNCaps]
    /\ pcbind \in [Channels -> [Ends -> Notifs \cup {NULL}]]
    /\ eopen  \in [Channels -> [Ends -> BOOLEAN]]

\* The refcount discipline: a notification is alive iff it has a reference
\* — a cap OR a channel hold. The inductive invariant the per-action
\* nlive updates must preserve; a slip that freed a still-referenced object
\* (or leaked a zero-referenced one) breaks it. ChannelFireSafe below is
\* its rev1§3.3 corollary, named separately because it is the property the
\* teardown firing relies on.
RefCountSound ==
    \A n \in Notifs : (n \in nlive) <=> (ncaps[n] + Holds(n) > 0)

\* rev1§3.3 firing safety: every peer-closed binding on a not-yet-reclaimed
\* channel names a LIVE notification, so the firing at close / whole-object
\* teardown signals a live object, never a freed one. Holds across
\* RevokeNotif precisely because the channel's own hold keeps the
\* notification alive.
ChannelFireSafe ==
    \A ch \in Channels, e \in Ends :
        (ChanAlive(ch) /\ pcbind[ch][e] /= NULL) => pcbind[ch][e] \in nlive

\* No hold outlives reclamation: a reclaimed channel pins no notification.
ReclaimedReleased ==
    \A ch \in Channels :
        ~ChanAlive(ch) => \A e \in Ends : pcbind[ch][e] = NULL

====
