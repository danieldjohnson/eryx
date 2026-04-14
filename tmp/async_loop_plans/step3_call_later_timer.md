# Step 3 — `call_at` / `call_later` via per-timer host subtasks

## Why

`_EryxLoop` currently stubs out `call_later` / `call_at` with
`NotImplementedError` (implicit — only `call_soon` is implemented). This means
`asyncio.sleep`, `asyncio.wait_for`'s timeout arm, `asyncio.timeout()`, and any
scheduled callback pattern all fail inside the sandbox.

Adding real timer support requires some kind of clock-aware wake primitive.
The cleanest approach that needs no wasmtime/CM-ABI changes is to add one
host-async function — `sleep` — that the guest calls like any other async
host import. Each `call_later` becomes a Python Task that awaits `sleep` and
then invokes the callback. Cancellation propagates through the fix from
Step 1, and shutdown cleans up via Step 2.

This is the "Option A" design: simpler than a Python-side timer heap
("Option B"), adequate for typical async workloads, and forward-compatible —
Option B would use the same `sleep` WIT import if later desired.

## What changed (final implementation)

### 1. WIT interface

New file `crates/eryx-runtime/wit/deps/timer.wit` (hardlinked into
`crates/eryx/wit/deps/timer.wit`):

```wit
package eryx:timer@0.1.0;

interface timer {
    /// Sleep for the given duration in nanoseconds.
    /// If delay-ns is 0, completes immediately.
    sleep: async func(delay-ns: u64);
}
```

Imported in `runtime.wit`:

```wit
import eryx:timer/timer@0.1.0;
```

**Design decisions:**

- **`delay-ns` (relative) instead of `deadline-ns` (absolute).** The
  original plan used absolute WASI monotonic-clock timestamps, but the host
  can't easily convert those back to a tokio sleep — `WasiCtx` doesn't
  expose the `Instant` epoch it captured at creation. Relative delays avoid
  the clock-epoch problem entirely, with negligible drift in a single-guest
  sandbox.

- **`async func` (CM-async) instead of sync `func` + fiber-based async.**
  TCP/TLS use fiber-based async (sync WIT + `func_wrap_async` in bindgen),
  which blocks the entire fiber. For timers that's fatal — `asyncio.sleep(1)`
  would freeze the entire event loop for 1 second. CM-async produces a
  subtask that integrates with the `_await_waitable` machinery, suspending
  only the awaiting coroutine while other tasks continue running.

### 2. Host implementation (`crates/eryx/src/wasm.rs`)

```rust
impl timer::Host for ExecutorState {}

impl timer::HostWithStore for HasSelf<ExecutorState> {
    fn sleep<T>(
        _accessor: &Accessor<T, Self>,
        delay_ns: u64,
    ) -> impl Future<Output = ()> + Send {
        async move {
            if delay_ns == 0 { return; }
            tokio::time::sleep(Duration::from_nanos(delay_ns)).await;
        }
    }
}
```

Uses the CM-async trait pattern (like `invoke`), not the fiber-based TCP
pattern. The sleep runs directly on tokio — no `NetRequest` channel needed
since there's no state to manage.

### 3. Preinit stub (`crates/eryx-runtime/src/preinit.rs`)

```rust
timer_instance.func_wrap_concurrent(
    "sleep",
    |_accessor: &Accessor<PreInitCtx>, (_delay_ns,): (u64,)| {
        Box::pin(async move { Ok(()) })
    },
)?;
```

Uses `func_wrap_concurrent` (like `invoke`), not `func_wrap_async` (TCP).

### 4. Guest Rust plumbing (`crates/eryx-wasm-runtime/src/lib.rs`)

```rust
pub enum TimerSleepResult { Ok, Pending(u32, u32) }

pub fn do_timer_sleep(delay_ns: u64) -> Result<TimerSleepResult, String> {
    // Uses call_import_async (CM-async), not call_import_sync (fiber).
    // Returns Pending(subtask_id, 0) if the sleep is in flight.
    // No PendingImportState needed — sleep has no return value to lift.
}
```

PyO3 wrapper `_eryx_timer_sleep(delay_ns)` returns `(status, value)`:
- status 0: completed immediately
- status 2: pending — value is `(subtask_id, 0)`

### 5. `_EryxLoop.call_at` / `call_later` / `_timer_handle_cancelled`

```python
def call_at(self, when, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.TimerHandle(when, callback, args, self, context)

    async def _run_timer():
        delay = max(0.0, when - self.time())
        delay_ns = int(delay * 1e9)
        if delay_ns > 0:
            result_type, value = _eryx._eryx_timer_sleep(delay_ns)
            if result_type == 2:  # Pending
                waitable, _promise = value
                await _await_waitable(waitable)
        if not handle._cancelled:
            handle._run()
        _async_state.get().timer_tasks.pop(handle, None)

    task_context = context.copy()
    task = self.create_task(_run_timer(), context=task_context)
    task_context[_async_state].timer_tasks[handle] = task
    return handle

def call_later(self, delay, callback, *args, context=None):
    if delay <= 0:
        return self.call_soon(callback, *args, context=context)
    return self.call_at(self.time() + delay, callback, *args, context=context)

def _timer_handle_cancelled(self, handle):
    state = handle._context[_async_state]
    task = state.timer_tasks.pop(handle, None)
    if task is not None:
        task.cancel()
```

**Key design decisions:**

- **`_AsyncState.timer_tasks: dict`** maps `TimerHandle → Task`. This keeps
  all async plumbing in one place. When a handle is cancelled, the loop's
  `_timer_handle_cancelled` callback (asyncio's official extension point)
  looks up and cancels the backing task.

- **No `TimerHandle` subclass.** Earlier approach subclassed `TimerHandle`
  to add a `_task` slot, but `TimerHandle` uses `__slots__` which made
  monkey-patching impossible. The map approach avoids this entirely —
  stock `TimerHandle`, cancellation via the official callback.

- **`context.copy()` for timer tasks.** The caller's context may already
  be entered (call_at invoked from inside a task step). Python forbids
  re-entering the same Context, so the timer task gets its own copy. Both
  contexts share the same `_async_state` since Context.copy() is shallow.

- **`call_later(0, ...)` fast path.** Delegates to `call_soon`, skipping
  the host sleep roundtrip. Matches CPython's `asyncio.sleep(0)` semantics
  (yield once, no real timer).

## Files touched

- `crates/eryx-runtime/wit/deps/timer.wit` — **new file**, hardlinked into
  `crates/eryx/wit/deps/timer.wit`.
- `crates/eryx-runtime/wit/runtime.wit` / `crates/eryx/wit/runtime.wit` —
  added `import eryx:timer/timer@0.1.0`.
- `crates/eryx/src/wasm.rs` — `timer::Host` + `timer::HostWithStore` impls,
  `use` import, comment in `bindgen!` (not listed in async imports since
  CM-async is automatic).
- `crates/eryx-runtime/src/preinit.rs` — `add_timer_stubs` function +
  call site.
- `crates/eryx-wasm-runtime/src/lib.rs` — `TimerSleepResult` enum,
  `do_timer_sleep` (CM-async via `call_import_async`).
- `crates/eryx-wasm-runtime/src/python.rs`:
  - PyO3 wrapper `_eryx_timer_sleep`, registered in `eryx_module`.
  - `_AsyncState`: added `timer_tasks: dict` field.
  - `_EryxLoop`: added `call_at`, `call_later`, `_timer_handle_cancelled`.
- `crates/eryx/tests/timer.rs` — **new file**, 6 integration tests.

## Tests

Tests are in `crates/eryx/tests/timer.rs`. Run with:
```bash
cargo test --test timer --features embedded
```

1. **`test_asyncio_sleep_basic`** — `await asyncio.sleep(0.05)` blocks for
   ~50ms wall clock.

2. **`test_asyncio_sleep_zero`** — `await asyncio.sleep(0)` completes
   immediately via `call_later(0, ...)` → `call_soon` fast path.

3. **`test_concurrent_sleeps_with_gather`** — three 50ms sleeps via
   `asyncio.gather` complete in ~50ms total (parallel, not sequential).
   Confirms CM-async suspends per-coroutine.

4. **`test_wait_for_timeout`** — `asyncio.wait_for(slow_callback(),
   timeout=0.05)` raises `TimeoutError`. Exercises `call_at` +
   `_timer_handle_cancelled` + Step 1 subtask cleanup.

5. **`test_timer_drained_on_shutdown`** — `asyncio.create_task(
   asyncio.sleep(10))` then main returns. Sandbox shuts down promptly
   (no 10s hang), no trap. Exercises Step 2 drain.

6. **`test_sleep_with_callback`** — `asyncio.sleep` followed by a host
   callback in the same execution. Confirms timer and callback subtasks
   coexist.

## Things to verify

- **Cache/build dance.** Adding a WIT interface invalidates the late-linking
  artifact cache. After the WIT edit, a fresh container build is needed
  (see `container/README.md`).

- **Preinit snapshot.** `crates/eryx-runtime/runtime.cwasm` is committed and
  embedded via `include_bytes!`. It must be rebuilt against the new WIT and
  re-committed — otherwise CI will have a stale snapshot.

- **`asyncio.sleep(0)` behaviour matches CPython.** `call_later(0, ...)`
  delegates to `call_soon`, which matches CPython's "yield once" semantics.
  Verified by `test_asyncio_sleep_zero`.

## Explicitly NOT in scope

- Don't implement other `AbstractEventLoop` stubs (`add_reader`,
  `run_in_executor`, etc.).
- Don't migrate to "Option B" (Python-side timer heap with rearmable single
  timer waitable). The WIT interface is forward-compatible — Option B would
  reuse `sleep` with a different guest-side scheduler — so nothing here
  paints us into a corner.
- Don't alter snapshot/restore behaviour for pending timers.

## Alternatives considered

- **Absolute deadline (`deadline-ns`) instead of relative delay.** The
  original plan used absolute WASI monotonic-clock timestamps. This avoids
  drift from processing latency, but requires converting guest timestamps
  back to host Instants. `WasiCtx` doesn't expose its epoch Instant, and
  storing a separate Instant at creation introduces two clocks that may
  diverge. Relative delays are simpler and sufficient for sandbox workloads.

- **Fiber-based async (sync WIT + `func_wrap_async`).** This is how TCP/TLS
  work. But fiber-based async blocks the entire event loop, which defeats
  the purpose of `asyncio.sleep()`. CM-async suspends only the awaiting
  coroutine.

- **`_EryxTimerHandle` subclass.** Earlier approach added a `_task` slot to
  a `TimerHandle` subclass and overrode `cancel()`. This works but
  `TimerHandle.__slots__` makes it awkward, and the map-based approach is
  cleaner — it uses the official `_timer_handle_cancelled` extension point
  and keeps all plumbing in `_AsyncState`.

- **Net handler channel for timer.** The original plan routed timer requests
  through the `NetRequest` channel like TCP. But timers have no state to
  manage — just a `tokio::time::sleep`. The host impl does the sleep
  directly in `timer::HostWithStore`, avoiding unnecessary channel plumbing.
