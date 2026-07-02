---- MODULE IpcReactor ----

\* Permission to use, copy, modify, and/or distribute this software for
\* any purpose with or without fee is hereby granted.
\* 
\* THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
\* WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
\* OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
\* FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
\* DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
\* AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
\* OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

\* Userspace IPC reactor: the lost-wakeup + backpressure protocol (spec rev2§3.3,
\* rev2§3.6).
\*
\* Models one channel between a sender and a receiver: the bounded FIFO queue and
\* TWO kernel notification words (faithful to kcore::notification — signal ORs a
\* bit in and either wakes the FIFO waiter or accumulates; wait consumes the word
\* if non-zero, else blocks). `word` is the on-readable notification (the receiver
\* waits on it); `wword` is the on-writable notification (a blocked sender waits on
\* it). The receiver runs the rev2§3.6 "bind, poll once, then wait" discipline and
\* the sender runs the symmetric rev2§3.3 backpressure discipline — both lost-wakeup
\* guards are now checked here, each with a committed, runnable negative control.
\*
\* The "bind, poll once, then wait" discipline (rev2§3.6, ipc/src/reactor.rs):
\*   - Register binds the source's events to the notification and then **self-
\*     signals** ("poll once") — IF the queue is already non-empty it sets `word`,
\*     surfacing a message that was queued *before* the bind (whose edge signal
\*     went nowhere). Without it, a send-before-bind message is slept through (the
\*     IpcReactor_NegControl.cfg negative control).
\*   - Send fires the on-readable binding **only when bound** (the receiver has
\*     registered). A Send before the bind still enqueues (no drop, rev2§3.3) but
\*     its edge signal is lost (`word` UNCHANGED) — exactly the send-before-bind
\*     hazard the poll-once exists to defeat.
\*   - The receiver runs `register -> loop { wait(); while recv_nb() {..} }`. It
\*     blocks on `word`, NOT on the queue: a queued message with `word = 0` is
\*     invisible to a blocked receiver. RecvBlock blocks iff `word = 0` (the wait-
\*     side lost-wakeup guard — see IpcReactor_NegLostWakeup.cfg), and RecvGet
\*     (recv_nb) drains only after `wait()` has returned (recv = "drain"), so the
\*     poll-once is genuinely load-bearing: gate the drain behind the wakeup and a
\*     missing poll-once strands delivery.
\*
\* Receiver control state `recv` is three-valued to model that loop faithfully:
\*   "poll"    = inside wait(), runnable (will block if word = 0, else wakes);
\*   "blocked" = wait() slept on a clear word;
\*   "drain"   = wait() returned, running the recv_nb drain loop.
\*
\* The symmetric backpressure discipline (rev2§3.3, ipc/src/endpoint.rs):
\*   - Send returns Full (never a drop) when the queue is full; the blocking sender
\*     (send_blocking) then waits on the on-writable notification `wword`. SendBlock
\*     blocks iff `wword = 0` (the writable lost-wakeup guard — see
\*     IpcReactor_NegBackpressure.cfg); SendWaitConsume consumes an accumulated
\*     writable signal and re-polls instead of blocking.
\*   - RecvGet (recv_nb success) frees a slot and fires the on-writable binding:
\*     it wakes a blocked sender (send' = "run", wword' = 0) or accumulates
\*     (wword' = 1) — the term-for-term mirror of Send's on-readable fire, faithful
\*     to recv_nb signalling on_writable after draining a slot.
\*
\* Scope: cap move/teardown safety is already CapRevocation.tla's
\* (MoveSemantics / FireSafe). This spec owns only the genuinely-new wakeup +
\* backpressure protocol the reactor introduces.
\*
\* Scope limitation (single source). This spec models ONE source on ONE on-readable
\* bit and ONE on-writable bit (word, wword in {0,1}). The reactor's MULTI-source
\* dispatch — the `used`-mask bit allocation, the `pending` drain, and the
\* `trailing_zeros` lowest-bit-first scan (ipc/src/reactor.rs) — is not modelled
\* here, and that lowest-bit ordering bias carries no fairness / starvation
\* property. The multi-source dispatch is proptest-routed; extending this
\* spec to multiple bits is a possible future step.
\*
\* Properties:
\*   Safety (the gate):
\*     - NoLostWakeup: a blocked receiver has nothing pending — no set on-readable
\*       bit. A lost wakeup is exactly a blocked receiver with work waiting and no
\*       pending signal to deliver it (rev2§3.6).
\*     - NoLostWakeupWritable: a blocked sender has a genuinely full queue and no
\*       set on-writable bit. A blocked sender with a free slot (or a missed
\*       writable signal) would be the symmetric lost wakeup (rev2§3.3).
\*     - NoDrop: every offered message is accounted for (received or still
\*       queued) — Full is the only refusal, never a silent drop (rev2§3.3).
\*     - FifoPerChannel: receive order = send order (rev2§3.3).
\*   Liveness (TLC-only; a bounded randomized search cannot establish it):
\*     - EventuallyDelivered: under weak fairness, every offered message is
\*       eventually received — no lost wakeup (readable or writable) strands
\*       delivery forever, even under genuine two-sided blocking.
\*
\* Negative controls (committed, runnable; the project's negative-control convention — the strongest
\* anti-theatre signal). Each lives in its own cfg (TLC admits one SPECIFICATION
\* per cfg) and reports the named property VIOLATED with a short trace:
\*   - IpcReactor_NegControl.cfg (SpecBadPoll): Register minus the poll-once self-
\*     signal -> NoLostWakeup false (blocked receiver, message queued before bind).
\*   - IpcReactor_NegBackpressure.cfg (SpecBadWritable): RecvGet minus the on-
\*     writable fire -> NoLostWakeupWritable false (blocked sender, slot freed).
\*   - IpcReactor_NegLostWakeup.cfg (SpecBadWait): RecvBlock minus the `word = 0`
\*     guard -> NoLostWakeup false (blocked receiver holding word = 1).

EXTENDS Naturals, Sequences

CONSTANTS
    MaxMsgs,     \* messages the sender offers — bounds the state space
    QueueDepth   \* channel queue capacity (rev2§3.2)

VARIABLES
    nextSend,    \* count of messages enqueued so far (ids 1..nextSend)
    queue,       \* Seq of in-flight message ids (FIFO, bounded by QueueDepth)
    recvd,       \* Seq of message ids the receiver has consumed (in order)
    word,        \* on-readable notification: 0 (clear) or 1 (readable bit set)
    recv,        \* receiver control state: "poll" | "blocked" | "drain"
    bound,       \* TRUE once the receiver has registered its on-readable binding
    wword,       \* on-writable notification: 0 (clear) or 1 (writable bit set)
    send         \* sender control state: "run" (runnable) | "blocked" (on Full)

vars == <<nextSend, queue, recvd, word, recv, bound, wword, send>>

Init ==
    /\ nextSend = 0
    /\ queue = << >>
    /\ recvd = << >>
    /\ word = 0
    /\ recv = "poll"
    /\ bound = FALSE
    /\ wword = 0
    /\ send = "run"

\* --- Actions -----------------------------------------------------------

\* The receiver registers its on-readable binding (rev2§3.6, reactor.rs:132-156):
\* it binds, then performs the poll-once self-signal — IF a message is already
\* queued it sets `word`, so the first wait() surfaces it instead of sleeping
\* through it. The hazard this defeats is reachable only because the binding is
\* not present from the start (bound = FALSE at Init), so a Send can precede it.
Register ==
    /\ ~bound
    /\ bound' = TRUE
    /\ word' = IF Len(queue) > 0 THEN 1 ELSE word
    /\ UNCHANGED <<nextSend, queue, recvd, recv, wword, send>>

\* The sender enqueues the next message; backpressure disables it when the queue
\* is full (Full, never a drop — the sender then takes SendBlock/SendWaitConsume).
\* The enqueue fires the persistent on-readable binding ONLY when bound: a blocked
\* receiver is woken and consumes the word (clearing it); a non-blocked receiver
\* just sees the word accumulate. When NOT bound, the message still enqueues but
\* the edge signal goes nowhere (`word` UNCHANGED) — the send-before-bind hazard.
Send ==
    /\ send = "run"
    /\ nextSend < MaxMsgs
    /\ Len(queue) < QueueDepth
    /\ nextSend' = nextSend + 1
    /\ queue' = Append(queue, nextSend + 1)
    /\ IF bound
       THEN IF recv = "blocked"
            THEN /\ recv' = "drain"   \* signal wakes the FIFO waiter ...
                 /\ word' = 0         \* ... which consumes the word, clearing it
            ELSE /\ recv' = recv
                 /\ word' = 1         \* no waiter: the word accumulates (OR-in)
       ELSE /\ recv' = recv
            /\ word' = word           \* edge signal of a not-yet-bound source lost
    /\ UNCHANGED <<recvd, bound, wword, send>>

\* Receiver in wait(), the accumulated on-readable word is set: wait() returns
\* immediately and consumes the word — proceed to the recv_nb drain loop.
RecvWake ==
    /\ recv = "poll"
    /\ bound
    /\ word = 1
    /\ recv' = "drain"
    /\ word' = 0
    /\ UNCHANGED <<nextSend, queue, recvd, bound, wword, send>>

\* Receiver in wait(), the on-readable word is clear: nothing pending — block.
\* The `word = 0` conjunct is the wait-side lost-wakeup guard (see the negative
\* control IpcReactor_NegLostWakeup.cfg): wait() checks the accumulated word before
\* sleeping, so a signal already delivered is never slept through. NOTE: the guard
\* is on the WORD, not the queue — wait() blocks on the notification, so a queued
\* message with word = 0 (a lost edge signal) is invisible to a blocked receiver.
RecvBlock ==
    /\ recv = "poll"
    /\ bound
    /\ word = 0
    /\ recv' = "blocked"
    /\ UNCHANGED <<nextSend, queue, recvd, word, bound, wword, send>>

\* Receiver draining (recv_nb success): consume the head. Draining is gated on
\* recv = "drain" — i.e. only after wait() returned — which is what makes the
\* poll-once load-bearing (a receiver cannot drain a queued message without first
\* being woken). Freeing a slot fires the on-writable binding: a blocked sender is
\* woken and consumes wword (clearing it); a running sender just sees it accumulate.
RecvGet ==
    /\ recv = "drain"
    /\ Len(queue) > 0
    /\ recvd' = Append(recvd, Head(queue))
    /\ queue' = Tail(queue)
    /\ IF send = "blocked"
       THEN /\ send' = "run"    \* writable signal wakes the blocked sender ...
            /\ wword' = 0        \* ... which consumes wword, clearing it
       ELSE /\ send' = send
            /\ wword' = 1        \* no blocked sender: wword accumulates (OR-in)
    /\ UNCHANGED <<nextSend, word, recv, bound>>

\* Receiver draining, queue empty (recv_nb returns Empty): the drain loop is done,
\* loop back to wait().
RecvDone ==
    /\ recv = "drain"
    /\ Len(queue) = 0
    /\ recv' = "poll"
    /\ UNCHANGED <<nextSend, queue, recvd, word, bound, wword, send>>

\* Sender on Full (send_blocking), the on-writable word is clear: nothing to send
\* into — block. The `wword = 0` conjunct is the writable lost-wakeup guard (the
\* symmetric twin of RecvBlock's `word = 0`; see IpcReactor_NegBackpressure.cfg).
SendBlock ==
    /\ send = "run"
    /\ nextSend < MaxMsgs
    /\ Len(queue) = QueueDepth
    /\ wword = 0
    /\ send' = "blocked"
    /\ UNCHANGED <<nextSend, queue, recvd, word, recv, bound, wword>>

\* Sender on Full but the accumulated on-writable word is set: the wait-for-
\* writable returns immediately and consumes wword — re-poll, do NOT block (the
\* writable mirror of RecvWake / the old RecvWaitConsume).
SendWaitConsume ==
    /\ send = "run"
    /\ nextSend < MaxMsgs
    /\ Len(queue) = QueueDepth
    /\ wword = 1
    /\ wword' = 0
    /\ UNCHANGED <<nextSend, queue, recvd, word, recv, bound, send>>

Next ==
    \/ Register
    \/ Send
    \/ RecvWake
    \/ RecvBlock
    \/ RecvGet
    \/ RecvDone
    \/ SendBlock
    \/ SendWaitConsume

\* Weak fairness on the progress actions only (not RecvBlock / SendBlock — blocking
\* is not progress). Register must fire (the receiver must bind); Send keeps
\* offering messages and waking the receiver; RecvWake/RecvGet/RecvDone run the
\* receive loop; SendWaitConsume clears an accumulated writable signal so a Full
\* sender re-polls. Together these discharge EventuallyDelivered under genuine
\* two-sided blocking.
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(Register)
    /\ WF_vars(Send)
    /\ WF_vars(RecvWake)
    /\ WF_vars(RecvGet)
    /\ WF_vars(RecvDone)
    /\ WF_vars(SendWaitConsume)

\* --- Invariants (safety) -----------------------------------------------

TypeOK ==
    /\ nextSend \in 0..MaxMsgs
    /\ Len(queue) \in 0..QueueDepth
    /\ word \in {0, 1}
    /\ recv \in {"poll", "blocked", "drain"}
    /\ bound \in BOOLEAN
    /\ wword \in {0, 1}
    /\ send \in {"run", "blocked"}
    /\ \A i \in 1..Len(queue) : queue[i] \in 1..MaxMsgs
    /\ \A i \in 1..Len(recvd) : recvd[i] \in 1..MaxMsgs

\* No lost wakeup (readable): a blocked receiver has nothing pending. Blocked with
\* a set on-readable word would mean an accumulated wakeup was slept through; and
\* in this protocol a blocked receiver always has an empty queue (a queued message
\* under a bound sender always set the word), so blocked with a queued message is
\* exactly a send-before-bind lost wakeup (rev2§3.6).
NoLostWakeup ==
    (recv = "blocked") => (Len(queue) = 0 /\ word = 0)

\* No lost wakeup (writable): a blocked sender has a genuinely full queue and no
\* set on-writable word. Blocked with a free slot (Len(queue) < QueueDepth) is a
\* missed writable wakeup — the symmetric defect (rev2§3.3).
NoLostWakeupWritable ==
    (send = "blocked") => (Len(queue) = QueueDepth /\ wword = 0)

\* No drop: every offered message is either received or still queued — Full is
\* the only refusal (rev2§3.3). With FIFO contiguity this is a counting identity.
NoDrop ==
    nextSend = Len(recvd) + Len(queue)

\* FIFO per channel: received in send order, with the queue holding the next
\* contiguous run (rev2§3.3).
FifoPerChannel ==
    /\ \A i \in 1..Len(recvd) : recvd[i] = i
    /\ \A i \in 1..Len(queue) : queue[i] = Len(recvd) + i

\* --- Liveness (TLC-only) -----------------------------------------------

\* Under weak fairness, every offered message is eventually received: no lost
\* wakeup strands delivery forever, even with the sender able to block on Full and
\* the receiver able to block on an empty notification. This is a liveness property
\* a bounded, randomized search cannot establish and TLC can.
EventuallyDelivered ==
    <>(Len(recvd) = MaxMsgs)

\* --- Negative controls (committed, runnable; the negative-control convention) ------
\* Each broken spec swaps exactly one action for a guard-stripped variant; under
\* it the named safety invariant MUST be reachable-false. Checked by its own cfg
\* (one SPECIFICATION per cfg). The real model (Spec) keeps every guard and all
\* invariants hold.

\* (1) Send-before-bind poll-once control (IpcReactor_NegControl.cfg). Register
\* WITHOUT the poll-once self-signal: it binds but never surfaces an already-queued
\* message (`word` UNCHANGED). Under SpecBadPoll, NoLostWakeup MUST be violated — a
\* message sent before the bind, then the receiver registers without surfacing it
\* and blocks on the clear word, holding a queued message. Mirrors the real-code
\* harness's documented control (model.rs: deleting register's self-signal
\* deadlocks the send-before-bind interleaving).
RegisterNoPoll ==
    /\ ~bound
    /\ bound' = TRUE
    /\ UNCHANGED <<nextSend, queue, recvd, word, recv, wword, send>>

NextBadPoll ==
    \/ RegisterNoPoll
    \/ Send
    \/ RecvWake
    \/ RecvBlock
    \/ RecvGet
    \/ RecvDone
    \/ SendBlock
    \/ SendWaitConsume

SpecBadPoll == Init /\ [][NextBadPoll]_vars

\* (2) Writable lost-wakeup control (IpcReactor_NegBackpressure.cfg). RecvGet
\* WITHOUT the on-writable fire: it drains a slot but never signals wword / wakes a
\* blocked sender. Under SpecBadWritable, NoLostWakeupWritable MUST be violated — a
\* blocked sender is left with a free slot and no writable signal. Mirrors the
\* real-code harness's documented control (model.rs: removing recv_nb's on_writable
\* signal makes the blocked sender hang).
RecvGetNoWritable ==
    /\ recv = "drain"
    /\ Len(queue) > 0
    /\ recvd' = Append(recvd, Head(queue))
    /\ queue' = Tail(queue)
    /\ UNCHANGED <<nextSend, word, recv, bound, wword, send>>

NextBadWritable ==
    \/ Register
    \/ Send
    \/ RecvWake
    \/ RecvBlock
    \/ RecvGetNoWritable
    \/ RecvDone
    \/ SendBlock
    \/ SendWaitConsume

SpecBadWritable == Init /\ [][NextBadWritable]_vars

\* (3) Wait-side lost-wakeup control (IpcReactor_NegLostWakeup.cfg), a runnable
\* artifact. RecvBlock WITHOUT the
\* `word = 0` guard: the receiver blocks without checking the accumulated word.
\* Under SpecBadWait, NoLostWakeup MUST be violated — a blocked receiver holding
\* word = 1 (a missed accumulated wakeup).
RecvBlockNoGuard ==
    /\ recv = "poll"
    /\ bound
    /\ recv' = "blocked"
    /\ UNCHANGED <<nextSend, queue, recvd, word, bound, wword, send>>

NextBadWait ==
    \/ Register
    \/ Send
    \/ RecvWake
    \/ RecvBlockNoGuard
    \/ RecvGet
    \/ RecvDone
    \/ SendBlock
    \/ SendWaitConsume

SpecBadWait == Init /\ [][NextBadWait]_vars

====
