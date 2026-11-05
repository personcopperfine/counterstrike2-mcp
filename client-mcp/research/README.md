# research/ — developer tooling (any language)

Scripts that support development of this server but are not part of the shipped binary:
recorders, judges, one-off probes, sig-DB helpers that call `../../idabins/scripts/*.py`.
Python is fine here (the idabins POC is Python). Rules: run with `python3 -I`, keep sources
in the repo (not /tmp), never embed addresses — call `sigcheck.py resolve` instead.
