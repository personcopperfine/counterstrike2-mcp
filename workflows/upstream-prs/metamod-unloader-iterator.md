# DRAFT (not filed): Metamod `Unloader` erases from `m_hooks` while iterating it

Status: draft for the user to review. Not filed.

Pins: metamod-source `170058a`, khook `0ca2b013`.

## Summary

`Unloader`'s constructor (`core/metamod_plugins.cpp:47-56`) walks `_khook->m_hooks` with a range-for and calls
`KHook::RemoveHook(id, true, &Unloader::HookRemoval, this)` for each id. If an id is no longer associated with a hook
(the plugin already removed it), async `RemoveHook` calls the removal function **synchronously**, on the same thread
(`third_party/khook/src/detour.cpp:1852-1860`). `Unloader::HookRemoval` then runs `m_hooks.erase(id)`
(`metamod_plugins.cpp:77-79`). That erases the element the range-for iterator points at, and the loop then increments
an invalidated iterator. This is undefined behaviour.

`KHookImpl` inserts every id it hands out into `m_hooks` (`core/metamod_khook.h:61, 90`). Its `RemoveHook`
(`:94-95`) forwards to KHook and never erases from `m_hooks`. So every id a plugin removed on its own before unload is
still in the set when the `Unloader` runs.

## Who hits it

Any Metamod plugin that removes one of its own KHook hooks before Metamod unloads it, for example in its `Unload`. That
is the normal, tidy path. With libstdc++, `std::set::erase` frees the node, and the `++it` that follows reads the
node's `right` link and, if that link is null, its `parent` link from freed memory. glibc's tcache overwrites the first
16 bytes of a freed chunk, which on x86-64 cover the node's colour and its `parent` link. So whether the loop survives
depends on the tree's shape.

## Suggested fix

Snapshot the ids before issuing any removal, for example:

```cpp
std::vector<KHook::HookID_t> ids(_khook->m_hooks.begin(), _khook->m_hooks.end());
for (auto id : ids) { if (id != KHook::INVALID_HOOK) KHook::RemoveHook(id, true, ..., this); }
```

Also: either have `KHookImpl::RemoveHook` erase the id from `m_hooks` (under the same mutex the `Unloader` uses), or
document that `m_hooks` is the set of every id the plugin was ever given.

## How we found it

While reviewing the cs2-mcp bridge (lane E, 2026-10-20), we read `metamod_plugins.cpp:46-88` against
`detour.cpp:1846-1860`. This has not been reproduced live yet. The bridge removes all of its hooks in `Unload`
(`remove_all_blocking`), so its `meta unload` goes through this path every time.
