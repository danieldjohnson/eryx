# Step 1 — Fix cancellation leak in host-waitable wrappers

## Why

The Python-side wrappers for host async calls (`await_invoke`, `_await_net_result`)
do not clean up if the surrounding Task is cancelled while they are suspended at
`await future`. When cancellation fires:

- `CancelledError` is raised inside the coroutine at the `await future` line.
- The coroutine unwinds without removing the entry from `state.futures[waitable]`.
- `_eryx.subtask_drop_(waitable)` is never called, so the host subtask stays live.
- When the host subtask eventually completes, `resume()` finds the orphan future,
  calls `set_result` on it, but nothing is awaiting — the result sits in `_eryx`
  memory and `promise_get_result_` is never called.
- Depending on timing, asyncio may call `_EryxLoop.call_exception_handler` on the
  unawaited future, which writes to `_loop.exception` and gets raised out of the
  next `poll()` — a cross-task surprise that can kill an unrelated execution.

This is a latent bug today affecting callbacks and TCP/TLS operations whenever
`asyncio.wait_for`, `asyncio.shield`, or any manual `Task.cancel()` is used. It
also blocks the clean-cancellation story for the upcoming timer feature.

## What to change

Introduce a shared helper that owns the await-and-cleanup pattern, and route all
existing wrappers through it:

```python
async def _await_waitable(waitable: int) -> None:
    """Join `waitable` to the current state's set and await completion.

    On cancellation or any exception before completion, drop the host subtask
    so the host can tear it down and the waitable set can eventually be dropped.
    """
    future = _loop.create_future()
    state = _async_state.get()
    state.futures[waitable] = future
    if state.waitable_set is None:
        state.waitable_set = _eryx.waitable_set_new_()
    _eryx.waitable_join_(waitable, state.waitable_set)
    try:
        await future
    except BaseException:
        state.futures.pop(waitable, None)
        _eryx.subtask_drop_(waitable)
        raise
```

Then rewrite `await_invoke` (roughly python.rs:1114) and `_await_net_result`
(roughly python.rs:1155) to use this helper in place of their ad-hoc
join-and-await blocks. The post-await "read the result" logic stays the same.

### Race to keep in mind

If the host subtask completes and `resume` sets the future's result just before
cancellation is delivered, `await future` returns normally and we skip the
except clause — correct. If cancellation wins, `subtask_drop_` on a
just-completed subtask is a defined no-op that frees the handle.

## Files to touch

- `crates/eryx-wasm-runtime/src/python.rs` — the `ERYX_ASYNC_INJECT_CODE` string
  literal starting around line 948. Specifically:
  - Add `_await_waitable` near the existing helpers.
  - Refactor `await_invoke` (line ~1114) to use it.
  - Refactor `_await_net_result` (line ~1155) to use it.

No Rust-side changes required. No WIT changes. No host-side changes.

## How to test

Add integration tests (probably under `crates/eryx/tests/`) that exercise
cancellation of in-flight operations:

1. **Callback cancel**: define a slow callback (sleeps 1s host-side), run
   ```python
   await asyncio.wait_for(slow_callback(), timeout=0.01)
   ```
   and assert `TimeoutError` is raised. Follow-up execute call should succeed,
   confirming no orphaned state.

2. **TCP cancel**: same shape, against a TCP connect to an unresponsive target,
   wrapped in `wait_for`.

3. **No subtask leak**: after the cancelled op, trigger a `waitable_set` drop
   (either by completing the execution or by instrumenting `state.futures`) and
   confirm the set is empty. If wasmtime traps on non-empty set drop, that's
   the forcing function — the test will fail loudly before this fix lands.

4. **Shield still works**: `asyncio.shield(callback())` should NOT propagate
   cancellation to the inner op. Make sure the helper doesn't break this
   (it shouldn't — cancellation only fires in the helper if the wrapping
   coroutine was actually cancelled).

Run via `mise run test` (nextest, fast path). The pre-fix version of test (1)
should either leak silently or trap at the next `waitable_set_drop_`; either
way it demonstrates the bug.

## Things to verify

- No behavioural change for the success path — `await_invoke` and network reads
  should still return identical values for non-cancelled executions. Run the
  existing test suite.
- `_EryxLoop.call_exception_handler` no longer fires unexpectedly. If existing
  tests were masking this, they should get quieter, not louder.
- The existing `parallel_callbacks` example (per README) still passes — this is
  the load-bearing non-cancellation case.
