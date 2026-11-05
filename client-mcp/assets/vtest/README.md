# assets/vtest: embedded test-script fragments and suites

Byte-identical copies of the vtest POC (`idabins/worknotes/vtest/`). Embedded with `include_str!` by
`src/testscript/mod.rs`; `stage` writes them to `<app>/vtest/` next to the rendered suite. Do not edit
them here: change the POC and copy again (`cmp` must stay silent).

| File | Source (relative to `cs2-docker/`) |
|---|---|
| `mark.vtest` | `idabins/worknotes/vtest/suites/mark.vtest` |
| `setup_player.vtest` | `idabins/worknotes/vtest/suites/setup_player.vtest` |
| `pos.vtest` | `idabins/worknotes/vtest/suites/pos.vtest` |
| `snap.vtest` | `idabins/worknotes/vtest/suites/snap.vtest` |
| `harness.txt` | `idabins/worknotes/vtest/suites/harness.txt` |
| `sandbox.txt` | `idabins/worknotes/vtest/suites/sandbox.txt` |
| `move_jump.txt` | `idabins/worknotes/vtest/suites/move_jump.txt` |

`movie_demo.txt` (needs `DEMO=`) is not embedded yet; see workflows/07.
