# Step 2 — Drain pending tasks on shutdown instead of trapping

## Why

`_AsyncState.pending_count` tracks exactly one thing: whether the top-level
coroutine passed to `run_async` has returned. It is initialised to 1 in
`run_async` and decremented once in `_return_result`. It does *not* account for
`asyncio.create_task(...)` calls the user makes, nor any future timer tasks,
nor siblings spawned by `asyncio.gather`.

Consequence: when the main coroutine returns while background tasks are still
live, `_poll` takes the `pending_count == 0` branch (python.rs:1104-1107) and
calls `_eryx.waitable_set_drop_(state.waitable_set)` on a set that still has
live waitables joined. Per the Component Model canonical ABI,
`waitable-set.drop` **traps if the set is non-empty**. The user has observed
this trap when a timeout task is still pending at main-script exit.

This matters on its own (clean shutdown of fire-and-forget tasks), and matters
doubly for Step 3 — the timer feature's canonical test case is "main returns
while a `call_later` is still pending," which hits exactly this path.

## What to change

Treat main coroutine completion as the **start** of shutdown, not the end.
Match `asyncio.runners.Runner.close()` semantics: cancel all live tasks, drain
them, then tear down the set.

### Track tasks in `_AsyncState`

Add a `tasks: set` field to `_AsyncState` and register every task in
`_EryxLoop.create_task()`. This gives us a reliable, self-contained set of
live tasks without depending on `asyncio.all_tasks()` (see "Alternatives
considered" below).

```python
@dataclass
class _AsyncState:
    waitable_set: Optional[int]
    futures: dict[int, asyncio.Future]
    handles: list[asyncio.Handle]
    pending_count: int
    tasks: set  # NEW — all tasks created under this state
```

```python
def create_task(self, coro, *, name=None, context=None):
    task = asyncio.Task(coro, loop=self, context=context)
    _async_state.get().tasks.add(task)
    return task
```

### Rewrite `_poll` drain logic

Rewrite the exit branch of `_poll` (python.rs ~line 1110) roughly as:

```python
def _poll(state: _AsyncState) -> int:
    _loop.poll(state)
    if state.pending_count == 0:
        # Main coroutine done — cancel any stragglers and drain them.
        stragglers = [t for t in state.tasks if not t.done()]
        if stragglers:
            for t in stragglers:
                t.cancel()
            # Run the cancellation handlers. Stragglers awaiting host waitables
            # will take the _await_waitable exception path (Step 1), which
            # does sync `subtask_cancel_` → `subtask_drop_` to release their
            # host subtasks.
            _loop.poll(state)
            # If anything is still awaiting a host event (e.g. shielded tasks),
            # return WAIT so wasmtime keeps resuming us until the set is empty.
            if state.futures:
                assert state.waitable_set is not None
                _eryx.context_set_(state)
                return WAIT | (state.waitable_set << 4)

        if state.waitable_set is not None:
            _eryx.waitable_set_drop_(state.waitable_set)
        return EXIT
    else:
        assert state.waitable_set is not None
        _eryx.context_set_(state)
        return WAIT | (state.waitable_set << 4)
```

Key points:

- **Tasks tracked explicitly in `_AsyncState`.** All task creation flows
  through `_EryxLoop.create_task()`, so `state.tasks` captures everything —
  both the internal `_return_result` wrapper and user `asyncio.create_task()`
  calls. This is self-contained and doesn't rely on asyncio internals.
- **Drain uses the existing WAIT/resume machinery.** We don't invent a new
  codepath; if stragglers are still awaiting host events after the first cancel
  round, we return WAIT and let wasmtime keep calling `resume`. Subsequent
  `_poll` calls will re-enter this branch (pending_count is still 0) and keep
  draining until `state.futures` is empty.
- **Depends on Step 1.** Without the `_await_waitable` fix (which does
  `subtask.cancel` → `subtask.drop` on the cancellation path), cancelling a
  straggler doesn't release its host subtask, so draining would never
  complete — and `waitable_set.drop` would trap at shutdown. This is why
  Step 1 ships first.
- **Preserves existing exception behaviour.** `_loop.exception` is still raised
  out of `poll()`, so genuine errors in stragglers aren't silently swallowed.

### Alternatives considered

- **`asyncio.all_tasks(_loop)`** — the original approach. asyncio maintains a
  global weak set of tasks internally, so `all_tasks()` would return them.
  However, this depends on CPython implementation details of how
  `asyncio.Task.__init__` registers into that weak set, and our custom
  `_EryxLoop` may not fully participate in all the bookkeeping asyncio expects.
  Tracking tasks ourselves in `_AsyncState` is more robust and keeps the async
  state self-contained in one place.

## Files to touch

- `crates/eryx-wasm-runtime/src/python.rs` — all within `ERYX_ASYNC_INJECT_CODE`:
  - `_AsyncState` dataclass: add `tasks: set` field
  - `_EryxLoop.create_task()`: register task into `_async_state.get().tasks`
  - `run_async()`: initialize `tasks=set()` in `_AsyncState` constructor
  - `_poll()`: replace immediate-exit with drain logic

No Rust-side, WIT, or host-side changes.

## How to test

1. **Fire-and-forget callback**:
   ```python
   asyncio.create_task(slow_callback())  # takes 1s
   # main returns immediately
   ```
   Should complete without trapping. The straggler task gets cancelled, its
   host subtask is dropped (via Step 1), set drop succeeds.

2. **Fire-and-forget TCP**: same shape, with a pending TCP read.

3. **Shielded straggler**: a background task that does
   `await asyncio.shield(something_slow())`. Cancellation shouldn't propagate
   through the shield; the drain should wait for the inner op (or time out —
   matching CPython, we can let it finish naturally and just log a warning if
   we want to bound it).

4. **Straggler that errored before main returned**: its exception should
   surface, not be silently dropped. Confirm `_loop.exception` still works.

5. **No stragglers**: existing happy-path tests must still behave identically.
   `state.futures` is empty, `waitable_set` drops cleanly, return EXIT.

6. **Stragglers that schedule new tasks during cancellation**: the drain loop
   should handle re-entry — keep polling until `all_tasks` reports everything
   done. Construct a task whose `CancelledError` handler does
   `asyncio.create_task(...)`.

## Things to verify

- The `_loop.exception` path still fires for real errors; only clean
  cancellation is suppressed.
- No regression in the existing session-persistence tests — `snapshot-state`
  and friends use the same `run_async` / `_poll` path.
- If you plan to log a warning about stragglers (CPython does), pick a channel
  that won't collide with user-visible stderr in normal tests.

## Sequencing note

This PR should land **after** Step 1, not simultaneously. The drain logic
relies on `_await_waitable` calling `subtask_drop_` on cancel. Stacking the
PRs keeps the dependency explicit for reviewers.
