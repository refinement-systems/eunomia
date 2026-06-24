The usual approach is to treat a slow TLA+/TLC run as either a **state explosion** problem or an **expensive expression evaluation** problem. Most serious wins come from reducing the number of reachable states; only after that is it worth optimizing expressions. TLC is an explicit-state checker, so it benefits directly from fewer generated/distinct states, while Apalache is symbolic and may have different bottlenecks. Apalache’s docs explicitly warn that SMT-based bottlenecks can differ from TLC intuition. ([Apalache][1])

For TLC, start with the Toolbox profiler or `-coverage`. The Toolbox profiler reports expression invocation counts, expression “cost,” and action metrics such as total states and distinct states produced by each action. Invocation count points to hot expressions; cost often points to expressions that force enumeration of large sets such as powersets or function sets; action metrics point to actions that generate too many successor states or too many duplicate successor states. Profiling has overhead, is not available for distributed TLC, and does not profile liveness properties, so use it on a representative smaller model and turn it off for final large runs. ([tla.msr-inria.inria.fr][2])

A reasonable TLC command-line profiling/debug run looks like this:

```sh
java -jar tla2tools.jar \
  -workers auto \
  -coverage 1 \
  -config MyModel.cfg \
  MySpec.tla
```

`-coverage n` prints coverage information every `n` minutes, and `-workers auto` uses as many worker threads as there are cores. TLC also exposes runtime counters through `TLCGet`, including generated states, distinct states, queue size, duration, level, and diameter, which is useful for ad-hoc instrumentation. ([GitHub][3]) ([GitHub][3])

The first speedup is usually smaller models: fewer nodes, fewer messages, smaller integer ranges, smaller buffers, fewer clients. This is not “cheating” if the small model still covers the shape of the protocol. It is common to keep multiple configs: a tiny “edit loop” model, a medium safety model, and a separate liveness model. Safety and liveness should often be checked separately; liveness is slower and can block some optimizations such as symmetry reduction. ([learntla.com][4])

The next big lever is abstraction. Replace details that do not affect the property with coarser values: `load \in 0..3` instead of `0..100`, `overloaded \in BOOLEAN` instead of exact load, abstract message payloads instead of real payloads, bounded queues instead of arbitrary queues. The point is to preserve distinctions relevant to the bug class being checked. A more detailed model can then be treated as a refinement of the abstract model. ([learntla.com][4])

Use symmetry sets when processes, replicas, clients, values, etc. are genuinely interchangeable. TLC can reduce the reachable state space by treating permutations of model values as equivalent; the Toolbox docs give the example of a three-value symmetry set reducing by up to `3!`. However, this is a soundness-sensitive optimization: TLC does not verify that your declared symmetry is valid, and an invalid symmetry declaration can hide errors. ([tla.msr-inria.inria.fr][5])

Reduce accidental nondeterminism. Common examples are: using a sequence when order does not matter, using a set when duplicates cannot matter, using a bag/multiset when duplicates matter but order does not, letting workers pick “any” item when a canonical item would be equivalent, or modeling a setup/loader process whose interleavings do not affect the property. Fusing two actions into one can also help, but only when the intermediate interleavings are not part of what you are trying to verify; otherwise it can hide exactly the concurrency bug TLA+ is meant to find. ([learntla.com][4]) ([learntla.com][4]) ([learntla.com][4])

Views are another powerful but dangerous reduction. A `VIEW` tells TLC what projection of the state to use when deciding whether states are equivalent. This is useful for ignoring auxiliary variables that do not affect the behavior or checked properties. It is also easy to make an unsound view that collapses genuinely different states, so I would reserve it for clearly auxiliary state and validate it against a non-view model on smaller constants. ([learntla.com][4]) ([learntla.com][4])

For expression-level speed, the biggest rule is: construct small sets directly; do not generate huge sets and filter them. For example, `[Workers -> SUBSET Items]` is already exponential in both dimensions. If only a tiny fraction of that function set is valid, define the valid choices in a constructive form instead of filtering the entire space. This may not change the number of reachable states, but it can dramatically reduce time per generated state. ([learntla.com][4])

Also watch for expensive helpers in hot actions or invariants. Sorts, permutations, transitive closures, recursive operators, large comprehensions, and nested quantifiers can dominate runtime. TLC supports definition overrides, and in extreme cases Java-level operator overrides can be orders of magnitude faster than a mathematically direct TLA+ definition. TLC also has `TLCEval`, which can help when TLC’s lazy evaluation causes an expression to be recomputed repeatedly. ([tla.msr-inria.inria.fr][6]) ([learntla.com][4]) ([GitHub][3])

State constraints and action constraints are useful, but I would treat them as modeling assumptions, not harmless performance flags. A state constraint stops TLC from exploring successors of states that do not satisfy it; an action constraint can mention primed variables too. That is appropriate for “the environment never sends more than N messages” or “this model is intentionally bounded,” but it can hide bugs if used just to cut away inconvenient regions of the graph. ([tla.msr-inria.inria.fr][6])

For Apalache, use `--smtprof` rather than TLC-style profiling. It writes `profile.csv` with source locations and measures such as SMT variables, constants, SMT expressions, and a computed weight; the docs also provide a heatmap script. Apalache’s tuning options include invariant-checking mode, transition/invariant filters, solver parameters, solver threads, random seeds, and SMT translation options such as short-circuiting. ([Apalache][7]) ([Apalache][8])

A practical workflow is:

1. Keep a simple, obviously correct spec/config as the reference.
2. Run a small baseline and record generated states, distinct states, diameter, runtime, and which properties are checked.
3. Enable the profiler or coverage on the small/medium model.
4. If distinct states are huge, reduce model detail, constants, nondeterminism, symmetry, or atomicity.
5. If generated states are huge but distinct states are not, inspect action enablement and duplicate-generating actions.
6. If expression cost dominates, rewrite set constructions, avoid huge enumerations, add overrides, or use `TLCEval`/instrumentation selectively.
7. Re-run the reference and optimized versions on small constants to check that the optimization did not change the intended reachable behavior or checked properties.
8. Only then scale constants, workers, memory, and possibly distributed TLC.

The main rule is: optimize the model’s **irrelevant distinctions**, not its **failure modes**. A good optimization makes TLC stop distinguishing states that your property cannot observe; a bad one removes the behavior that would have exposed the bug.

[1]: https://apalache-mc.org/docs/apalache/index.html "Getting Started - Apalache Documentation"
[2]: https://tla.msr-inria.inria.fr/tlatoolbox/doc/model/profiling.html "Profiling"
[3]: https://github.com/tlaplus/tlaplus/blob/master/general/docs/current-tools.md "tlaplus/general/docs/current-tools.md at master · tlaplus/tlaplus · GitHub"
[4]: https://learntla.com/topics/optimization.html "Optimizing Model Checking — Learn TLA+"
[5]: https://tla.msr-inria.inria.fr/tlatoolbox/doc/model/model-values.html "Model Values and Symmetry"
[6]: https://tla.msr-inria.inria.fr/tlatoolbox/doc/model/spec-options-page.html "Spec Options Page"
[7]: https://apalache-mc.org/docs/apalache/profiling.html "Profiling Your Specification - Apalache Documentation"
[8]: https://apalache-mc.org/docs/apalache/tuning.html "Fine Tuning - Apalache Documentation"

