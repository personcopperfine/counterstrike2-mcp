# KV3 text fixtures

Copied test data from the MIT-licensed
[ValveKeyValue](https://github.com/ValveResourceFormat/ValveKeyValue) repository,
**commit 28ca0e5** ("Bump version"), path
`ValveKeyValue/ValveKeyValue.Test/Test Data/TextKV3/` — upstream file names kept.

They drive `tests/vkv_conformance.rs`, the port of upstream's KV3 text-syntax test suite.

Not copied (their upstream tests are skipped, see that file's header):

* `root_*` / `inf_nan_serialized.kv3` — they exercise `Document`s whose root value is not an
  object (`cs2_kv3::Document.root` is an `Object` by design; see the crate docs);
* `array_kv1.vdf`, `flagged_value_kv1.vdf`, `array_from_kv1.kv3`, `flagged_value_from_kv1.kv3`
  — KV1 <-> KV3 conversion tests.
