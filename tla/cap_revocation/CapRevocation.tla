---- MODULE CapRevocation ----
\* Kernel capability revocation model for Eunomia OS (spec §2.2, §3.4).
\*
\* State to model:
\*   - Capability derivation tree (CDT): each cap slot has an optional parent
\*   - Per-process cspace: set of live cap slot ids
\*   - Channel queues: sequences of in-flight messages, each holding cap slots
\*   - A "revoking" set — slots currently being revoked
\*
\* Key invariant (spec §2.2):
\*   After revoke(cap) completes, no slot in any cspace or any channel queue
\*   is a descendant of cap in the CDT.
\*   "Revocation sees through queues" — in-flight caps are unconditionally
\*   destroyed, with no "except messages in flight" caveat.
\*
\* Actions to model:
\*   Copy(src, dst)        - derive a child cap from src into dst slot
\*   Send(ch, msg, caps)   - move caps from cspace into channel queue slot
\*   Receive(ch, msg)      - move caps from queue into receiver cspace
\*   Revoke(cap)           - walk CDT, delete all descendants including queued
\*   Retype(cap)           - only valid after revoke ensures exclusivity
\*
\* This spec must be model-checked (TLC) before M1 implementation begins.

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    CapIds,     \* finite set of cap slot IDs
    Procs,      \* set of process IDs
    Channels,   \* set of channel IDs
    NULL        \* null/empty slot sentinel

VARIABLES
    parent,     \* CapIds -> CapIds \cup {NULL}  (CDT parent pointer)
    cspaces,    \* Procs -> SUBSET CapIds  (live caps per process)
    queues,     \* Channels -> Seq(SUBSET CapIds)  (in-flight cap sets)
    revoked     \* SUBSET CapIds  (caps deleted this revoke; ghost variable)

vars == <<parent, cspaces, queues, revoked>>

\* All descendants of cap in the CDT (recursive).
RECURSIVE Descendants(_)
Descendants(cap) ==
    LET children == {c \in CapIds : parent[c] = cap}
    IN  children \cup UNION {Descendants(c) : c \in children}

Init ==
    /\ parent   = [c \in CapIds |-> NULL]
    /\ cspaces  = [p \in Procs  |-> {}]
    /\ queues   = [ch \in Channels |-> << >>]
    /\ revoked  = {}

\* --- Actions (stubs — fill in before M1) ---

\* Copy: install a child cap derived from src into dst.
CopyCap(src, dst) ==
    /\ \E p \in Procs : src \in cspaces[p]  \* src must be live
    /\ parent[dst] = NULL                    \* dst must be free
    /\ parent' = [parent EXCEPT ![dst] = src]
    /\ \E p \in Procs :
        cspaces' = [cspaces EXCEPT ![p] = cspaces[p] \cup {dst}]
    /\ UNCHANGED <<queues, revoked>>

\* TODO: Send, Receive, Revoke, Retype

Next ==
    \/ \E s, d \in CapIds : CopyCap(s, d)
    \* \/ Send(...) \/ Receive(...) \/ \E c \in CapIds : Revoke(c)

Spec == Init /\ [][Next]_vars

\* --- Invariants ---

TypeOK ==
    /\ \A c \in CapIds : parent[c] \in CapIds \cup {NULL}
    /\ \A p \in Procs  : cspaces[p] \subseteq CapIds

\* A cap must not appear in both a cspace and a channel queue (move semantics).
\* (Full uniqueness invariant deferred until Send/Receive actions are modelled.)
NoDoubleOwnership ==
    \A p1, p2 \in Procs :
        p1 # p2 => cspaces[p1] \cap cspaces[p2] = {}

====
