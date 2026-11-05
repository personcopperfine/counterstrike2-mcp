# Protocol changes in m3.15: `input.set` release+tap of a held button

Bridge build `m3.15` adds no method and no param (branch `streamline/wp16-playtest`, WP1.6;
ti_convoy FIXME 26). `m3.13` belongs to the console/cvar work package (WP1.3).

## What was wrong

`input.set {release:[b], tap:[b]}` while `b` is held (we own its +1 delta) was a no-op for the
game: `release` cleared the held bit, `tap` set it again, and the per-frame body sends one delta
per bit whose wanted state differs from the applied state. The wanted state never differed, so no
-1 and no second +1 went out: the button stayed down, the game saw no new press (a held `attack`
fires nothing more). The workaround was two calls with a wait between them.

## Now

The bridge remembers the bits released and tapped in the same call while it owned their +1
(`InState.regap`). The next frame forces them up (a real -1); the tap starts on the frame after
and lasts its full `frames`. Unrelated bits in the same call behave as before. `input.clear`,
a failed list (rolled back) and a disarm reset the memory. A `release` without a `tap` of the same
button, and a `tap` of a button that is not held, are unchanged.

Capability: none to gate on (the method set is the same); `hello.build` is `m3.15`. Against an
older bridge the old behaviour remains (the tools do not paper over it).

Test: `crates/cs2-bridge/tests/ops_hooks.rs`
`input_release_and_tap_of_a_held_button_in_one_call_re_presses_it` (failed before the fix with
deltas `[1, 1]`, passes with `[1, -1, 1, 1, ...]`). Not run live.

## Not changed

While an `angles` override is set, console `setang` is ignored (the override wins every tick).
Documented in the `input_set` descriptions; `input_clear` or `angles: null` hands the view back.
