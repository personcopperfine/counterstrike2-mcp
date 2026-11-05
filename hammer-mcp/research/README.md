# research/ — developer tooling (any language)

Scripts that support development of this server but are not part of the shipped binary:
recorders, judges, one-off probes, sig-DB helpers that call `../../idabins/scripts/*.py`.
Python is fine here (the idabins POC is Python). Rules: run with `python3 -I`, keep sources
in the repo (not /tmp), never embed addresses — call `sigcheck.py resolve` instead.

The directory also holds the RE notes the tools and the bridge were built from (their
addresses are the sig-DB entries, cited — not re-derived):
[props-re.md](props-re.md) (the prop/model/transform/connection facts, bridge m2.18),
[triggers-re.md](triggers-re.md) (the trigger/brush-entity facts: tie/untie, saved format,
material, compile + runtime, bridge m2.19) and [sounds-re.md](sounds-re.md) (the sound tool
facts: the compile route, the runtime loading, the drain positions from the server's packed
params, bridge m2.9/m2.20).
