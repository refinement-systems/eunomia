---- MODULE CapRevocation ----
\* Kernel capability revocation model for Eunomia OS (spec rev2§2.2, rev2§3.4,
\* rev2§5.1, rev2§6).
\*
\* Models the capability derivation tree (CDT), per-process cspaces,
\* channel queues holding in-flight caps, TCB on-exit/on-fault binding
\* slots holding notification caps, copy/send/receive/revoke/retype/
\* bind/thread-death.
\*
\* Two specifications share this module and its variables. Spec is the CDT
\* revocation model (the bulk below). TSpec is the rev2§3.3 channel
\* whole-object teardown model — peer-closed bindings and their firing —
\* checked by CapRevocation_Teardown.cfg; see its own header further down.
\* Each spec keeps the other's variables constant, so they do not multiply
\* each other's state space.
\*
\* Key properties checked:
\*   - MoveSemantics: at every instant a live cap has exactly one owner —
\*     a process cspace, a queue slot, or a TCB binding slot, never two
\*     (spec rev2§3.4).
\*   - LiveParent: a live cap's CDT parent is live. Together with
\*     DeadNowhere this is "revoke destroys all descendants" in invariant
\*     form: no descendant can survive its ancestor's revocation anywhere,
\*     channel queues and TCB binding slots included — unconditionally,
\*     no "except messages in flight" caveat (spec rev2§2.2).
\*   - DeadNowhere: a deleted cap appears in no cspace, no queue slot,
\*     and no binding slot.
\*   - FireSafe: a non-NULL binding slot always names a live cap. This is
\*     the rev2§5.1 firing obligation in invariant form: revoking the
\*     notification's lineage racing thread-death must either leave a
\*     live cap for the firing to signal through (the cap holds a ref, so
\*     its object is live) or have cleared the slot (signaling nothing is
\*     a no-op) — never a freed object. It must hold at every preemption
\*     point of the future preemptible revoke walk, which is exactly why
\*     the slots are CDT-visible rather than refcounted raw pointers.
\*   - RevokedDead: ghost check — a revoked slot stays dead until reused
\*     by a fresh Copy.
\*   - ReportMonotone (action property): the terminal report transitions
\*     at most once, running -> exited | faulted (rev2§5.1).
\*   - EventuallyRevoked (liveness): a started revoke completes — once a
\*     root is marked `revoking`, its subtree eventually empties. Holds
\*     under weak fairness on RevokeStep BECAUSE the Copy guard forbids
\*     derivation into a revoking subtree, so the subtree only shrinks
\*     (the revoke marker/guard, mechanized — rev2§2.2 "restartable").
\*
\* TSpec properties (rev2§3.3 channel teardown):
\*   - ChannelFireSafe: every peer-closed binding on a live channel names a
\*     live notification — the teardown firing signals a live object, never
\*     a freed one, even when the notification's whole cap lineage was
\*     revoked (the channel's hold keeps it alive).
\*   - RefCountSound: a notification is alive iff a cap or a channel hold
\*     references it — the refcount discipline the kernel maintains.
\*   - ReclaimedReleased: a reclaimed channel holds no notification.
\*
\* Modeling notes:
\*   - Revoke is modeled STEPWISE, not atomically: RevokeBegin marks
\*     the root (`revoking`), RevokeStep deletes ONE leaf descendant, and
\*     RevokeEnd clears the marker once the subtree is empty — the three
\*     interleave with every other action. The leaf-first / DFS-post-order
\*     deletion order the kernel walk must respect (so LiveParent holds at
\*     every preemption point) is therefore now a CHECKED invariant under
\*     arbitrary interleaving, not just a recorded obligation: deleting a
\*     childless cap orphans nobody. Completion across the gaps between
\*     quanta is the EventuallyRevoked liveness property, which holds
\*     because the Copy guard (`~AncestorOrSelfRevoking`) — the model of
\*     the kernel's derive guard — forbids re-growth below a revoking root. Two
\*     committed negative controls pin both load-bearing choices: a
\*     non-leaf RevokeStepBad violates LiveParent (CapRevocation_NegControl.cfg);
\*     dropping the Copy guard livelocks EventuallyRevoked
\*     (CapRevocation_NegLiveness.cfg).
\*   - Receivers tolerate sparse messages: revoke deletes caps out of
\*     queued messages in place, so a message can arrive with fewer caps
\*     than were sent — the "null cap slots" of rev2§3.4.
\*   - Retype models only its authority precondition: it requires that the
\*     cap has no live descendants (exclusivity proven by revoke, rev2§2.2).
\*   - Bind moves the cap out of the binder's cspace into the TCB slot
\*     (rev2§3.4 move semantics; the kernel caller duplicates first to keep
\*     access). Kernel rebind = delete-the-displaced-cap + bind; single-
\*     cap delete (children re-parent one level up) preserves LiveParent
\*     by construction and is not modeled. Channel destruction's queued-cap
\*     cleanup is the same shape and likewise unmodeled here; its NEW
\*     content — peer-closed firing under whole-object teardown — is the
\*     separate TSpec at the foot of this file (rev2§3.3).
\*   - Thread destruction deletes binding caps with ordinary CDT cleanup
\*     and produces no report and no firing — destruction is the parent
\*     acting, not the thread dying (rev2§5.1).

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    CapIds,     \* finite set of cap slot IDs
    Procs,      \* set of process IDs
    Channels,   \* set of channel IDs
    Threads,    \* set of TCB IDs (each carrying exit/fault binding slots)
    Notifs,     \* notification object IDs (TSpec; rev2§3.3 channel teardown)
    QueueDepth, \* per-channel queue capacity (donated at creation, rev2§3.2)
    NULL        \* null/empty sentinel (model value)

MaxCapsPerMsg == 4  \* spec rev2§3.1: 4 cap slots per message

BindKinds == {"exit", "fault"}  \* the two fixed TCB binding slots (rev2§5.1)

Ends     == {0, 1}  \* the two channel endpoints (TSpec)
MaxNCaps == 2       \* bound on outstanding caps per notification (TSpec)

VARIABLES
    live,       \* SUBSET CapIds — allocated cap slots
    parent,     \* CapIds -> CapIds \cup {NULL}  (CDT parent pointer)
    cspaces,    \* Procs -> SUBSET CapIds  (caps owned by each process)
    queues,     \* Channels -> Seq(SUBSET CapIds)  (in-flight cap sets)
    bindings,   \* Threads -> [BindKinds -> CapIds \cup {NULL}]
    treport,    \* Threads -> {"running", "exited", "faulted"}  (rev2§5.1)
    revoked,    \* SUBSET CapIds — ghost: revoked ids not yet reused
    revoking,   \* SUBSET CapIds — roots with an in-progress revoke (revoke marker)
    \* --- TSpec only (channel teardown, rev2§3.3); constant under Spec -------
    nlive,      \* SUBSET Notifs — alive notification objects
    ncaps,      \* Notifs -> 0..MaxNCaps  (outstanding caps per notif)
    pcbind,     \* Channels -> [Ends -> Notifs \cup {NULL}]  (peer-closed)
    eopen       \* Channels -> [Ends -> BOOLEAN]  (endpoint has a live cap)

\* The revocation half (Spec) and the channel-teardown half (TSpec) carry
\* disjoint variables; each half holds the other's constant, so neither
\* multiplies the other's state space.
crVars == <<live, parent, cspaces, queues, bindings, treport, revoked,
            revoking>>
tdVars == <<nlive, ncaps, pcbind, eopen>>
vars   == <<live, parent, cspaces, queues, bindings, treport, revoked,
            revoking, nlive, ncaps, pcbind, eopen>>

\* All descendants of cap in the CDT (recursive). Dead caps have parent
\* NULL, so the walk only ever finds live caps.
RECURSIVE Descendants(_)
Descendants(cap) ==
    LET children == {c \in CapIds : parent[c] = cap}
    IN  children \cup UNION {Descendants(c) : c \in children}

\* A live cap with no live child — a leaf of the live CDT forest. RevokeStep
\* may only delete a leaf, so deleting it orphans nobody (the leaf-first
\* obligation the kernel walk must respect — rev2§2.2).
IsLeaf(l) == l \in live /\ ~\E x \in live : parent[x] = l

\* Walk the CDT parent chain up from x; TRUE iff any node on the path,
\* including x itself, is a revoke root. The TLA mirror of the verified
\* exec ancestor-walk `ancestor_or_self_revoking`; terminates on the
\* acyclic `parent` forest (the same assumption Descendants relies on).
RECURSIVE AncestorOrSelfRevoking(_)
AncestorOrSelfRevoking(x) ==
    IF x = NULL THEN FALSE
    ELSE IF x \in revoking THEN TRUE
    ELSE AncestorOrSelfRevoking(parent[x])

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
    /\ revoking = {}
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
    /\ ~AncestorOrSelfRevoking(src)  \* derive guard: no growth into a
                                     \* revoking subtree (keeps revoke terminating)
    /\ live'    = live \cup {dst}
    /\ parent'  = [parent EXCEPT ![dst] = src]
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup {dst}]
    /\ revoked' = revoked \ {dst}
    /\ UNCHANGED <<queues, bindings, treport, revoking>>

\* Send: caps move from the sender's cspace into a queue slot (rev2§3.4).
\* Non-blocking; disabled (FULL) when the queue is at capacity (rev2§3.3).
\* cs is a set of the sender's own caps: the cs \subseteq cspaces[p] conjunct
\* is the move guard, so Next enables Send by quantifying cs over
\* SUBSET cspaces[p] — exactly the sets that can pass that guard — rather than
\* over the whole SUBSET CapIds and discarding the rest.
Send(p, ch, cs) ==
    /\ cs /= {}
    /\ cs \subseteq cspaces[p]
    /\ Cardinality(cs) <= MaxCapsPerMsg
    /\ Len(queues[ch]) < QueueDepth
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \ cs]
    /\ queues'  = [queues EXCEPT ![ch] = Append(@, cs)]
    /\ UNCHANGED <<live, parent, bindings, treport, revoked, revoking>>

\* Receive: head message's surviving caps land in the receiver's cspace.
\* (Cspace-slot exhaustion makes receive fail with the message left
\* queued, rev2§3.3 — equivalent to this action simply not being taken.)
Receive(p, ch) ==
    /\ queues[ch] /= << >>
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup Head(queues[ch])]
    /\ queues'  = [queues EXCEPT ![ch] = Tail(@)]
    /\ UNCHANGED <<live, parent, bindings, treport, revoked, revoking>>

\* Bind: configure a TCB binding slot (rev2§5.1) — the cap moves from the
\* binder's cspace into the slot, exactly as Send moves caps into queue
\* slots. Binding is legal regardless of the thread's report state (a
\* late binding simply never fires).
Bind(p, t, k, c) ==
    /\ c \in cspaces[p]
    /\ bindings[t][k] = NULL
    /\ bindings' = [bindings EXCEPT ![t][k] = c]
    /\ cspaces'  = [cspaces EXCEPT ![p] = @ \ {c}]
    /\ UNCHANGED <<live, parent, queues, treport, revoked, revoking>>

\* Thread death (rev2§5.1): the terminal record transitions exactly once,
\* and the kernel fires the matching binding slot at this instant — it
\* reads bindings[t][k] and signals through the cap if one is present.
\* The firing itself moves no caps; its safety is the FireSafe invariant
\* holding in the pre-state of this action.
ThreadExit(t) ==
    /\ treport[t] = "running"
    /\ treport' = [treport EXCEPT ![t] = "exited"]
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, revoked, revoking>>

ThreadFault(t) ==
    /\ treport[t] = "running"
    /\ treport' = [treport EXCEPT ![t] = "faulted"]
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, revoked, revoking>>

\* Revoke is modeled STEPWISE: RevokeBegin marks the root, RevokeStep
\* deletes ONE leaf descendant per preemption point, RevokeEnd clears the
\* marker once the subtree is empty. c itself stays live (seL4 semantics:
\* revoke empties the subtree below the cap, establishing exclusivity for
\* retype).

\* Delete exactly one cap d everywhere it can reside — cspace, queue slot,
\* binding slot — clear its parent and ghost-revoke it. Shared by the
\* leaf-first RevokeStep and the negative-control RevokeStepBad; the ONLY
\* difference between those two is whether d is required to be a leaf.
DeleteOne(d) ==
    /\ live'    = live \ {d}
    /\ parent'  = [parent EXCEPT ![d] = NULL]
    /\ cspaces' = [p \in Procs |-> cspaces[p] \ {d}]
    /\ queues'  = [ch \in Channels |->
                      [i \in 1..Len(queues[ch]) |-> queues[ch][i] \ {d}]]
    /\ bindings' = [t \in Threads |-> [k \in BindKinds |->
                      IF bindings[t][k] = d THEN NULL ELSE bindings[t][k]]]
    /\ revoked' = revoked \cup {d}
    /\ UNCHANGED <<treport, revoking>>

\* Mark the root c (only when it has descendants and is not already marked):
\* one preemption-point-free step that sets the marker, deletes nothing.
RevokeBegin(c) ==
    /\ c \in live
    /\ Descendants(c) /= {}
    /\ c \notin revoking
    /\ revoking' = revoking \cup {c}
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, treport, revoked>>

\* One bounded quantum: delete a single LEAF descendant. Leaf-only deletion
\* is what keeps LiveParent true at every interleaved state — deleting a
\* childless cap orphans nobody (the leaf-first / DFS-post-order obligation).
RevokeStep(c) ==
    /\ c \in revoking
    /\ \E l \in Descendants(c) :
        /\ IsLeaf(l)
        /\ DeleteOne(l)

\* Clear the marker once the subtree is empty; c survives.
RevokeEnd(c) ==
    /\ c \in revoking
    /\ Descendants(c) = {}
    /\ revoking' = revoking \ {c}
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, treport, revoked>>

\* Retype: consume an exclusive cap (e.g. untyped -> kernel object).
\* Sound only when no derived caps exist anywhere — the guard the kernel
\* establishes by running revoke first (rev2§2.2). Caps parked in binding
\* slots are not in any cspace, so they cannot be retyped; as CDT
\* residents they still block an ancestor's retype via Descendants.
Retype(p, c) ==
    /\ c \in cspaces[p]
    /\ Descendants(c) = {}
    /\ live'    = live \ {c}
    /\ parent'  = [parent EXCEPT ![c] = NULL]
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \ {c}]
    /\ revoked' = revoked \cup {c}
    /\ UNCHANGED <<queues, bindings, treport, revoking>>

Next ==
    /\ \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
       \/ \E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(p, ch, cs)
       \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
       \/ \E p \in Procs, t \in Threads, k \in BindKinds, c \in CapIds : Bind(p, t, k, c)
       \/ \E t \in Threads : ThreadExit(t)
       \/ \E t \in Threads : ThreadFault(t)
       \/ \E c \in CapIds : RevokeBegin(c)
       \/ \E c \in CapIds : RevokeStep(c)
       \/ \E c \in CapIds : RevokeEnd(c)
       \/ \E p \in Procs, c \in CapIds : Retype(p, c)
    /\ UNCHANGED tdVars

\* Weak fairness on RevokeStep: a marked root's subtree is eventually
\* drained. Combined with the Copy guard (the subtree never re-grows) this
\* gives EventuallyRevoked. Fairness does not change which states are
\* reachable, so the safety invariants are still checked over the full graph.
\* The subscript is crVars, not vars: RevokeStep is a revocation-half action
\* that specifies exactly the crVars primes (the tdVars are held constant by
\* this Spec, so WF_crVars and WF_vars coincide here, but WF_vars is rejected
\* by TLC because RevokeStep names no tdVars prime).
Fairness == \A c \in CapIds : WF_crVars(RevokeStep(c))

Spec == Init /\ [][Next]_vars /\ Fairness

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
    /\ revoking \subseteq CapIds

\* Where a cap currently resides.
ProcPlaces(c)  == {p \in Procs : c \in cspaces[p]}
QueuePlaces(c) == {<<ch, i>> \in Channels \X (1..QueueDepth) :
                      i <= Len(queues[ch]) /\ c \in queues[ch][i]}
BindPlaces(c)  == {<<t, k>> \in Threads \X BindKinds : bindings[t][k] = c}

\* rev2§3.4: exactly one owner at every instant — a cspace, a queue slot, or
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

\* rev2§5.1's firing obligation: a configured binding slot names a live cap
\* — so a thread-death firing signals a live object (the cap's ref keeps
\* the notification alive) or skips a cleared slot, never touches a
\* freed one. Implied by DeadNowhere; named because it is the property
\* the kernel's preemptible revoke walk must preserve at every step.
FireSafe ==
    \A t \in Threads, k \in BindKinds :
        bindings[t][k] = NULL \/ bindings[t][k] \in live

\* Ghost: revoked slots stay dead until explicitly reused.
RevokedDead == revoked \cap live = {}

\* rev2§5.1: at most one terminal report per thread, ever — the record never
\* leaves a terminal state.
ReportMonotone ==
    [][\A t \in Threads :
        treport[t] /= "running" => treport'[t] = treport[t]]_vars

\* --- Liveness ----------------------------------------------------------

\* rev2§2.2 "restartable": a started revoke completes. Once a root is marked
\* `revoking`, its subtree eventually empties. Holds under WF on RevokeStep
\* BECAUSE the Copy guard forbids re-derivation into a revoking subtree, so
\* the subtree shrinks monotonically and WF forces the leaves drained. The
\* committed liveness negative control (CapRevocation_NegLiveness.cfg) drops
\* the guard and livelocks this property — the runnable proof it has teeth.
EventuallyRevoked ==
    \A c \in CapIds : (c \in revoking) ~> (Descendants(c) = {})

\* --- Symmetry (invariant-only cfgs only) -------------------------------
\* The processes in Procs are interchangeable model values: every action and
\* invariant treats them uniformly, and the only initial asymmetry — the
\* arbitrary CHOOSE that picks which process holds the root cap (InitProc) — is
\* itself symmetric, since any process serves equally. So permuting Procs
\* carries every behaviour to an equivalent one, and SYMMETRY collapses each
\* permutation-orbit to a single representative: a sound state-space quotient
\* that checks the same behaviours in fewer states. Sound ONLY when no temporal
\* property is checked — TLC's symmetry reduction is unsound under liveness — so
\* this is named by the invariant-only CapRevocation_Safety.cfg (and its
\* symmetric negative control), NEVER by the liveness CapRevocation.cfg.
ProcSymmetry == Permutations(Procs)

\* --- Negative controls (committed) -------------------------------------
\* Each control action is the real action MINUS exactly one load-bearing
\* conjunct; a passing main model plus a failing control proves that
\* conjunct is load-bearing. Driven by the alternate specs below, checked by
\* CapRevocation_NegControl.cfg (safety) and CapRevocation_NegLiveness.cfg
\* (liveness).

\* SAFETY control: RevokeStep WITHOUT the IsLeaf filter — deletes any
\* descendant, leaf or interior. An interior delete leaves the deleted
\* node's live children pointing at a dead parent -> LiveParent CEX.
RevokeStepBad(c) ==
    /\ c \in revoking
    /\ \E l \in Descendants(c) : DeleteOne(l)

NextBad ==
    /\ \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
       \/ \E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(p, ch, cs)
       \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
       \/ \E p \in Procs, t \in Threads, k \in BindKinds, c \in CapIds : Bind(p, t, k, c)
       \/ \E t \in Threads : ThreadExit(t)
       \/ \E t \in Threads : ThreadFault(t)
       \/ \E c \in CapIds : RevokeBegin(c)
       \/ \E c \in CapIds : RevokeStepBad(c)
       \/ \E c \in CapIds : RevokeEnd(c)
       \/ \E p \in Procs, c \in CapIds : Retype(p, c)
    /\ UNCHANGED tdVars

SpecBad == Init /\ [][NextBad]_vars

\* LIVENESS control: Copy WITHOUT the ~AncestorOrSelfRevoking guard —
\* re-derivation into a revoking subtree is allowed, so a
\* RevokeStep<->CopyNoGuard cycle re-grows the subtree forever ->
\* EventuallyRevoked livelock (still fair: RevokeStep fires infinitely).
CopyNoGuard(p, src, dst) ==
    /\ src \in cspaces[p]
    /\ dst \notin live
    /\ live'    = live \cup {dst}
    /\ parent'  = [parent EXCEPT ![dst] = src]
    /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup {dst}]
    /\ revoked' = revoked \ {dst}
    /\ UNCHANGED <<queues, bindings, treport, revoking>>

NextNoGuard ==
    /\ \/ \E p \in Procs, s, d \in CapIds : CopyNoGuard(p, s, d)
       \/ \E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(p, ch, cs)
       \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
       \/ \E p \in Procs, t \in Threads, k \in BindKinds, c \in CapIds : Bind(p, t, k, c)
       \/ \E t \in Threads : ThreadExit(t)
       \/ \E t \in Threads : ThreadFault(t)
       \/ \E c \in CapIds : RevokeBegin(c)
       \/ \E c \in CapIds : RevokeStep(c)
       \/ \E c \in CapIds : RevokeEnd(c)
       \/ \E p \in Procs, c \in CapIds : Retype(p, c)
    /\ UNCHANGED tdVars

SpecNoGuard == Init /\ [][NextNoGuard]_vars /\ Fairness

\* =======================================================================
\* Channel whole-object teardown and peer-closed firing (rev2§3.3) — TSpec
\* =======================================================================
\* The rev2§3.3 teardown rule: deleting one endpoint fires the surviving
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
\* channel if separately funded" (rev2§3.3), and — the property that makes the
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

\* Revoke a notification's WHOLE cap lineage at once (rev2§2.2). The object
\* survives iff a channel still holds it — the refcount-keeps-alive
\* property that makes a teardown firing safe after the holder's caps die.
RevokeNotif(n) ==
    /\ n \in nlive
    /\ ncaps[n] > 0
    /\ ncaps' = [ncaps EXCEPT ![n] = 0]
    /\ nlive' = IF Holds(n) = 0 THEN nlive \ {n} ELSE nlive
    /\ UNCHANGED <<pcbind, eopen>>

\* Configure an endpoint's peer-closed binding (rev2§3.6): the channel takes a
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

\* Symmetry set for the teardown arm: the notification objects in Notifs are
\* interchangeable model values — no TSpec action or invariant names a specific
\* one, and Init seeds them symmetrically (nlive={}, ncaps all 0) — so
\* Permutations(Notifs) is a sound quotient. Named by CapRevocation_Teardown.cfg
\* only; TSpec checks no liveness property. See ProcSymmetry for the rationale.
NotifSymmetry == Permutations(Notifs)

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
\* its rev2§3.3 corollary, named separately because it is the property the
\* teardown firing relies on.
RefCountSound ==
    \A n \in Notifs : (n \in nlive) <=> (ncaps[n] + Holds(n) > 0)

\* rev2§3.3 firing safety: every peer-closed binding on a not-yet-reclaimed
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
