---- MODULE IpcReactor ----
\* Userspace IPC reactor: the lost-wakeup + backpressure protocol (spec §3.3,
\* §3.6; plan doc/plans/2_ipc.md §5.1).
\*
\* Models one channel between a sender and a receiver: the bounded FIFO queue,
\* and the kernel notification word (faithful to kcore::notification — signal
\* ORs a bit in and either wakes the FIFO waiter or accumulates; wait consumes
\* the word if non-zero, else blocks). The receiver runs the §3.6 "bind, poll
\* once, then wait" discipline: it blocks only when the queue is empty AND the
\* notification word is clear, and every enqueue fires the persistent on-readable
\* binding, which wakes a blocked receiver or accumulates for a polling one.
\*
\* Scope: cap move/teardown safety is already CapRevocation.tla's
\* (MoveSemantics / FireSafe). This spec owns only the genuinely-new wakeup +
\* backpressure protocol — the design risk the reactor (plan §4.2) introduces.
\*
\* Scope limitation (single source; review doc/results/19_ipc-review.md gap #5).
\* This spec models ONE source on ONE notification bit (word in {0,1}), and the
\* Loom fragment (ipc model.rs reactor_no_lost_wakeup_loom) likewise drives a
\* single source. The reactor's MULTI-source dispatch — the `used`-mask bit
\* allocation, the `pending` drain, and the `trailing_zeros` lowest-bit-first scan
\* (ipc/src/reactor.rs) — is exercised only by Shuttle harness #5 (fairness_smoke,
\* a best-effort smoke), and that lowest-bit ordering bias has NO fairness /
\* starvation property in any tier. Recorded rather than modelled: extending this
\* spec (or Loom) to multiple bits is a deliberate future step, not an MVP gate.
\*
\* Properties (plan §5.1, framing "both"):
\*   Safety (the gate; ports to the §5.2 Shuttle harness):
\*     - NoLostWakeup: a blocked receiver has nothing pending — no queued
\*       message, no set notification bit. A lost wakeup is exactly a blocked
\*       receiver with work waiting and no pending signal to deliver it.
\*     - NoDrop: every offered message is accounted for (received or still
\*       queued) — Full is the only refusal, never a silent drop (§3.3).
\*     - FifoPerChannel: receive order = send order (§3.3).
\*   Liveness (TLC-only; Shuttle's bounded randomized search cannot establish it):
\*     - EventuallyDelivered: under weak fairness, every offered message is
\*       eventually received — no lost wakeup strands delivery forever.
\*
\* Negative control (the loom-fence-removal discipline): delete RecvBlock's
\* `word = 0` conjunct — block without checking the word — and NoLostWakeup
\* becomes reachable-false; TLC reports a blocked receiver holding word = 1 (a
\* missed accumulated wakeup). Confirmed during bring-up, then reverted.

EXTENDS Naturals, Sequences

CONSTANTS
    MaxMsgs,     \* messages the sender offers — bounds the state space
    QueueDepth   \* channel queue capacity (§3.2)

VARIABLES
    nextSend,    \* count of messages enqueued so far (ids 1..nextSend)
    queue,       \* Seq of in-flight message ids (FIFO, bounded by QueueDepth)
    recvd,       \* Seq of message ids the receiver has consumed (in order)
    word,        \* notification word: 0 (clear) or 1 (readable bit set)
    recv         \* receiver control state: "poll" (runnable) | "blocked"

vars == <<nextSend, queue, recvd, word, recv>>

Init ==
    /\ nextSend = 0
    /\ queue = << >>
    /\ recvd = << >>
    /\ word = 0
    /\ recv = "poll"

\* --- Actions -----------------------------------------------------------

\* The sender enqueues the next message; backpressure disables it when the queue
\* is full (Full, never a drop). The enqueue fires the persistent on-readable
\* binding (§3.6): a blocked receiver is woken and receives the word (clearing
\* it); a polling receiver just sees the word accumulate.
Send ==
    /\ nextSend < MaxMsgs
    /\ Len(queue) < QueueDepth
    /\ nextSend' = nextSend + 1
    /\ queue' = Append(queue, nextSend + 1)
    /\ IF recv = "blocked"
       THEN /\ recv' = "poll"   \* signal wakes the FIFO waiter ...
            /\ word' = 0         \* ... which receives the word, clearing it
       ELSE /\ recv' = recv
            /\ word' = 1         \* no waiter: the word accumulates (OR-in)
    /\ UNCHANGED recvd

\* Receiver, runnable, queue non-empty: consume the head (recv_nb success).
RecvGet ==
    /\ recv = "poll"
    /\ Len(queue) > 0
    /\ recvd' = Append(recvd, Head(queue))
    /\ queue' = Tail(queue)
    /\ UNCHANGED <<nextSend, word, recv>>

\* Receiver, runnable, queue empty but the word is set (a signal accumulated):
\* wait() returns immediately and consumes the word — re-poll, do NOT block.
RecvWaitConsume ==
    /\ recv = "poll"
    /\ Len(queue) = 0
    /\ word = 1
    /\ word' = 0
    /\ UNCHANGED <<nextSend, queue, recvd, recv>>

\* Receiver, runnable, queue empty AND word clear: nothing pending — block.
\* The `word = 0` conjunct is the lost-wakeup guard (see the negative control in
\* the header): it is what makes wait() check the accumulated word before
\* sleeping, so a signal already delivered is never slept through.
RecvBlock ==
    /\ recv = "poll"
    /\ Len(queue) = 0
    /\ word = 0
    /\ recv' = "blocked"
    /\ UNCHANGED <<nextSend, queue, recvd, word>>

Next ==
    \/ Send
    \/ RecvGet
    \/ RecvWaitConsume
    \/ RecvBlock

\* Weak fairness on the progress actions only (not RecvBlock — blocking is not
\* progress). Send keeps offering messages and waking the receiver; RecvGet
\* drains the queue; RecvWaitConsume clears accumulated signals.
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Send)
    /\ WF_vars(RecvGet)
    /\ WF_vars(RecvWaitConsume)

\* --- Invariants (safety) -----------------------------------------------

TypeOK ==
    /\ nextSend \in 0..MaxMsgs
    /\ Len(queue) \in 0..QueueDepth
    /\ word \in {0, 1}
    /\ recv \in {"poll", "blocked"}
    /\ \A i \in 1..Len(queue) : queue[i] \in 1..MaxMsgs
    /\ \A i \in 1..Len(recvd) : recvd[i] \in 1..MaxMsgs

\* No lost wakeup: a blocked receiver has nothing pending. Blocked with a queued
\* message or a set word would mean a wakeup was lost (§3.6).
NoLostWakeup ==
    (recv = "blocked") => (Len(queue) = 0 /\ word = 0)

\* No drop: every offered message is either received or still queued — Full is
\* the only refusal (§3.3). With FIFO contiguity this is a counting identity.
NoDrop ==
    nextSend = Len(recvd) + Len(queue)

\* FIFO per channel: received in send order, with the queue holding the next
\* contiguous run (§3.3).
FifoPerChannel ==
    /\ \A i \in 1..Len(recvd) : recvd[i] = i
    /\ \A i \in 1..Len(queue) : queue[i] = Len(recvd) + i

\* --- Liveness (TLC-only) -----------------------------------------------

\* Under weak fairness, every offered message is eventually received: no lost
\* wakeup strands delivery forever. This is the property the §5.2 Shuttle
\* harness (bounded, randomized) cannot establish and TLC can.
EventuallyDelivered ==
    <>(Len(recvd) = MaxMsgs)

====
