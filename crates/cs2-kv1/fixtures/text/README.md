Test fixtures for the KV1 text reader/writer tests: all 73 `*.vdf` files of
`ValveKeyValue.Test/Test Data/Text/`.

Copied verbatim from the upstream port source:

* `ValveKeyValue` commit `28ca0e5` (https://github.com/ValveResourceFormat/ValveKeyValue),
  directory `ValveKeyValue/ValveKeyValue/ValveKeyValue.Test/Test Data/Text/`, MIT licence,
  © ValveKeyValue Contributors.

The upstream file names are kept (upstream refers to them as `Text.<case>.vdf`, minus the
directory). The tests read them from this directory at run time: `../tests/text_reader.rs`
(reading, including the escaped, comment, conditional, `#base`/`#include` and malformed
cases) and `../tests/text_writer.rs` (the writer's expected byte streams, the round-trip
census and the escape-on-write cases).

Two things about the files themselves matter to a reader of the code:

* Most of them carry a UTF-8 BOM. `parse` skips one leading U+FEFF, the way upstream's
  `StreamReader` consumes it before the reader sees a character, and `write` never emits one.
* They are hand-written for the reader, not produced by the writer: alignment tabs between a
  key and its value, single-slash comments and unquoted tokens all appear. Only
  `non_circular_list.vdf` happens to have exactly the shape `write` produces, which is why the
  writer's own output is pinned by the four `*_expected.vdf` files instead (see
  `text_writer.rs`).
