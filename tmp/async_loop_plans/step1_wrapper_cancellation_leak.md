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

    On cancellation or any exception before completion, resolve-then-drop the
    host subtask so the host can tear it down and the waitable set can
    eventually be dropped.
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
        # `subtask.drop` traps on an unresolved subtask — we must resolve
        # via sync `subtask.cancel` first, then drop.
        _eryx.subtask_cancel_(waitable)
        _eryx.subtask_drop_(waitable)
        raise
```

Then rewrite `await_invoke` (roughly python.rs:1114) and `_await_net_result`
(roughly python.rs:1155) to use this helper in place of their ad-hoc
join-and-await blocks. The post-await "read the result" logic stays the same.

### The `subtask.cancel` → `subtask.drop` sequence

An earlier draft of this plan claimed "`subtask_drop_` on a just-completed
subtask is a defined no-op." That is only correct for *resolved* subtasks —
for a pending/in-flight subtask, wasmtime's Component Model ABI traps with
`cannot drop a subtask which has not yet resolved`. The integration tests
confirmed this (see `crates/eryx/tests/async_cancellation_cleanup.rs`).

The canonical ABI intrinsic for this is `[subtask-cancel]`, which requests
cancellation and returns a final status code (2=RETURNED, 3=STARTED_CANCELLED,
4=RETURNED_CANCELLED). All three states are "resolved" — after cancel returns,
`subtask.drop` is safe. wit-bindgen uses exactly this pattern (see its
`rt/async_support/subtask.rs` — the `[subtask-cancel]` / `[subtask-drop]`
link_name pair).

This requires one host-side plumbing change on top of the Python-only change
the plan originally described:

- `crates/eryx-wasm-runtime/src/lib.rs` — add the `[subtask-cancel]` extern
  alongside the existing `[subtask-drop]` one.
- `crates/eryx-wasm-runtime/src/python.rs` — expose it as
  `_eryx.subtask_cancel_` via `#[pyfunction]` + `wrap_pyfunction!` (mirroring
  `subtask_drop_`).

### Why the sync cancel intrinsic is fine

The CM ABI offers two cancel flavours: sync (`[subtask-cancel]`, blocks until
the subtask resolves) and async (`[async-subtask-cancel]`, returns pending and
signals completion via the waitable set later). Sync is simpler and sufficient
because:

- wasmtime's sync cancel drops the host future immediately — for tokio futures
  (TCP/TLS/timers/`TypedCallback`s), drop runs destructors that unwind fast.
  The intrinsic returns promptly.
- The sandbox's execution-timeout guard (`ResourceLimits.execution_timeout`)
  is a hard backstop. Even if a pathological callback somehow blocked drop, the
  whole store is torn down at the deadline.
- wit-bindgen ships the sync flavour as its default precisely because, absent
  Rust linear types / async destructors, the async flavour is hard to make
  sound.

### Callback cancellation does NOT flow to user Python code

A consequence of the current `eryx-python` design: when the guest cancels a
subtask that represents a `PythonCallback::invoke` call (see
`crates/eryx-python/src/callback.rs:171-222`), wasmtime drops the outer future,
which drops the `JoinHandle` from `tokio::task::spawn_blocking`. Per tokio's
semantics, **dropping a `JoinHandle` detaches the blocking task — it does NOT
abort it**. The user's Python callback keeps running to completion on the
blocking pool; for async callbacks, `asyncio.run(coro)` drives the coroutine
to completion in its isolated event loop. The result is computed and then
dropped because nobody is awaiting.

Net behaviour:

- **No deadlock.** The guest's `subtask_cancel_` returns as soon as the outer
  future is dropped. It does *not* wait for the detached blocking task.
- **User callbacks have no cancellation signal.** No `CancelledError` is
  injected into their coroutine. Side effects (DB writes, HTTP requests) run
  to completion even after guest-side cancellation.
- **GIL latency afterwards.** The detached blocking thread holds the GIL until
  the callback returns; the next `Python::attach` from the guest path has to
  wait. Long callbacks → observable follow-up latency, not incorrectness.
- **Thread leak in worst case.** A callback that awaits something genuinely
  unresolvable (e.g. `asyncio.Event().wait()` with no releaser) keeps a
  blocking-pool thread alive indefinitely. Tokio's default blocking pool is
  512 threads, so the failure mode is gradual starvation, not immediate hang.

Truly cooperative callback cancellation would require reworking
`PythonCallback::invoke` to hold an `AbortHandle`-style drop hook that injects
`CancelledError` into the Python coroutine (for async callbacks) or sets a
thread-local cancel flag the callback can poll (for sync). This is a separate
redesign; explicitly out of scope for Step 1. The eryx-python module docs'
"Async Callback Limitations" section is the right place to note the current
behaviour when we write this up for users.

## Files to touch

- `crates/eryx-wasm-runtime/src/lib.rs` — add the `[subtask-cancel]` extern
  alongside the existing `[subtask-drop]` one (signature:
  `fn subtask_cancel(task: u32) -> u32`).
- `crates/eryx-wasm-runtime/src/python.rs`:
  - Expose `subtask_cancel_` as a `#[pyfunction]` and register it in
    `eryx_module` (mirrors `subtask_drop_`).
  - In the `ERYX_ASYNC_INJECT_CODE` string literal starting around line 948:
    - Add `_await_waitable` near the existing helpers.
    - Refactor `await_invoke` (line ~1114) to use it.
    - Refactor `_await_net_result` (line ~1155) to use it.

No WIT changes. No other host-side changes. (The original plan claimed "no
Rust-side changes"; that was incorrect — see the `subtask.cancel` section
above.)

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
