The main rule is: separate “make TLC do less work” from “change what theorem TLC is checking”. For liveness, it is very easy to accidentally do the latter.

Liveness is expected to be much more expensive than safety. TLC has to reason about whole infinite behaviors/cycles, not just bad finite prefixes; Lamport’s tutorial describes liveness properties as properties whose violation may require looking at the entire behavior. TLC’s own liveness implementation involves strongly connected component search, and the TLA+ project notes that this part uses a sequential Tarjan-style algorithm, so it does not scale like ordinary parallel state exploration. ([Leslie Lamport's Home Page][1]) ([GitHub][2])

A good first command-line shape is:

```sh
java -Xmx16G -cp tla2tools.jar tlc2.TLC \
  -workers auto \
  -lncheck final \
  -coverage 1 \
  -config Live.cfg \
  Spec
```

`-lncheck final` is often the first liveness-specific switch to try. By default, TLC performs liveness checks periodically as the number of distinct states grows; with `final`, it waits until the complete state graph has been computed. This does not change the property being checked, but it can delay finding a liveness counterexample and is bad if the state graph will never fit. `-workers auto` can still speed up state generation, though not the sequential SCC part. `-coverage 1` gives action/operator coverage every minute so you can see where TLC is spending time. ([TLA+ Wiki][3])  ([TLA+ Wiki][4])

Use two models: one large safety model and one smaller liveness model. In the safety model, check invariants, type invariants, refinement-style safety properties, and use symmetry if applicable. In the liveness model, remove symmetry and shrink constants. This is not just a performance convention: the Toolbox documentation explicitly warns that symmetry sets should not be used when checking liveness, because TLC may miss real errors or report spurious ones. ([tla.msr-inria.inria.fr][5]) Learn TLA+ also recommends separate safety and liveness models because liveness is slower and prevents symmetry-set optimization. ([learntla.com][6])

For profiling, split `Next` into named actions and avoid hiding all work behind one giant disjunction. TLC coverage reports how often each action generates a new state and helps identify both dead actions and expensive model-checking paths. The Toolbox profiler can also show how often operators and expression branches are called and how much they cost. ([TLA+ Wiki][4]) ([learntla.com][7])

The safest spec-level optimizations are local refactorings that leave `Init`, `Next`, fairness, and the checked property semantically unchanged. Put cheap guards before expensive expressions; restrict quantifiers to the smallest finite domains; avoid constructing huge sets and then filtering them; prefer point updates like `[f EXCEPT ![i] = v]` over enumerating whole functions; and remove variables that are purely derived from other variables, replacing them with operators. TLC compares states by variable values, so every extra stored bookkeeping variable can multiply the graph even if it is logically redundant.

Be careful with `VIEW`. It can be a real optimization, because it changes which states TLC treats as equal, but that is exactly why it is dangerous. The Toolbox documentation says a `VIEW` expression replaces the normal full-variable state identity; Learn TLA+ warns that used poorly it can “completely wreck your spec.” For liveness, it is especially risky because hidden variables may affect enabledness, fairness, or whether a liveness cycle exists. Use it only when you have a convincing abstraction/refinement argument, not as a casual speed knob. ([learntla.com][7])

Avoid using state/action constraints as final “proof” optimizations for liveness. A state constraint tells TLC not to explore successors of states that fail the constraint, and an action constraint similarly filters transitions; that means you are checking a different behavior graph. This is fine for bounded bug-hunting or for a deliberately restricted model, but it can hide the very cycle that would violate liveness. ([tla.msr-inria.inria.fr][8])

For liveness formulas themselves, split independent properties into separate TLC runs. If the same `Spec` satisfies `Live1` and satisfies `Live2`, then it satisfies their conjunction, but TLC often has less temporal-automaton/SCC bookkeeping to do when checking one at a time. Also review fairness carefully: changing `SF` to `WF`, moving fairness from per-process actions to a disjunction, or strengthening assumptions can make the property easier or harder, but it changes the theorem. Lamport’s tutorial gives the key distinction: strong fairness requires progress if a step is enabled infinitely often, while weak fairness only requires it if enabled continuously; strong fairness is therefore a stronger behavioral assumption. ([Leslie Lamport's Home Page][1])

The most powerful reductions usually come from abstraction. Use small constants for liveness runs, opaque model values instead of unnecessary integers/records, small message domains, fewer processes, and collapsed setup phases. For example, if some “loader” or initialization process just creates an arbitrary initial distribution, replace the interleaving setup process with a nondeterministic `Init` choice over the final distributions. That removes irrelevant interleavings without removing any steady-state behavior you care about. But treat this as an abstraction proof obligation: TLC is then verifying the abstract model, not automatically the larger concrete one.

A practical workflow is:

1. Run safety-only on the largest model you can, with symmetry if valid.
2. Run liveness on a much smaller model, no symmetry, usually with `-lncheck final`.
3. Use `-coverage`/profiler to find expensive actions and expressions.
4. Refactor expressions and remove derived state variables.
5. Split liveness properties and check them separately.
6. Only then consider abstraction, `VIEW`, or constraints, and document why they preserve the property you care about.

The fastest “optimization” that is usually wrong is adding fairness, symmetry, constraints, or a narrower `VIEW` until the liveness check passes. The right target is a smaller but behaviorally representative graph, with the same liveness theorem or a clearly justified abstraction of it.

[1]: https://lamport.azurewebsites.net/tla/tutorial/session9.html "PlusCal Tutorial - Session 9"
[2]: https://github.com/tlaplus/tlaplus/blob/master/general/docs/contributions.md "tlaplus/general/docs/contributions.md at master · tlaplus/tlaplus · GitHub"
[3]: https://docs.tlapl.us/using%3Atlc%3Astart "using:tlc:start - TLA+ Wiki"
[4]: https://docs.tlapl.us/using%3Acoverage "using:coverage - TLA+ Wiki"
[5]: https://tla.msr-inria.inria.fr/tlatoolbox/doc/model/model-values.html "Model Values and Symmetry"
[6]: https://learntla.com/topics/optimization.html "Optimizing Model Checking — Learn TLA+"
[7]: https://learntla.com/topics/toolbox.html "Using the Toolbox — Learn TLA+"
[8]: https://tla.msr-inria.inria.fr/tlatoolbox/doc/model/spec-options-page.html "Spec Options Page"

