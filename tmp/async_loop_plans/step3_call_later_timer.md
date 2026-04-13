# Step 3 — `call_at` / `call_later` via per-timer host subtasks

## Why

`_EryxLoop` currently stubs out `call_later` / `call_at` with
`NotImplementedError` (implicit — only `call_soon` is implemented). This means
`asyncio.sleep`, `asyncio.wait_for`'s timeout arm, `asyncio.timeout()`, and any
scheduled callback pattern all fail inside the sandbox.

Adding real timer support requires some kind of clock-aware wake primitive.
The cleanest approach that needs no wasmtime/CM-ABI changes is to add one
host-async function — `sleep-until` — that the guest calls like any other
async host import. Each `call_later` becomes a Python Task that awaits
`sleep-until` and then invokes the callback. Cancellation propagates through
the fix from Step 1, and shutdown cleans up via Step 2.

This is the "Option A" design: simpler than a Python-side timer heap
("Option B"), adequate for typical async workloads, and forward-compatible —
Option B would use the same `sleep-until` WIT import if later desired.

## What to change

### 1. WIT interface

Add a new interface at `crates/eryx-runtime/wit/` (new file, e.g.
`deps/timer/timer.wit` following the `net.wit` pattern):

```wit
package eryx:timer@0.1.0;

interface timer {
    /// Sleep until the given WASI monotonic-clock instant (nanoseconds).
    /// If the deadline is already past, completes immediately.
    sleep-until: async func(deadline-ns: u64);
}
```

Import it in the sandbox world (runtime.wit:8):

```wit
world sandbox {
    import eryx:net/tcp@0.1.0;
    import eryx:net/tls@0.1.0;
    import eryx:timer/timer@0.1.0;
    ...
}
```

**Clock rationale.** `deadline-ns` is measured in WASI
`monotonic-clock.now()` units — the same units backing CPython's
`time.monotonic()` on WASI. Wasmtime implements the WASI monotonic clock by
storing an `Instant` at store creation and returning
`initial.elapsed().as_nanos()`; the host's `tokio::time::Instant` is backed
by the same OS monotonic clock, differing only in epoch. The host impl of
`sleep-until` thus converts the WASI instant to a tokio `Instant` via the
store's `initial` (one extra clock read) and calls
`tokio::time::sleep_until`. Absolute deadlines don't drift if there's
latency between the guest issuing the call and the host starting the sleep,
and match how CPython's `BaseEventLoop` internally schedules (`_scheduled`
holds absolute times).

Note: WASI `monotonic-clock` also offers `subscribe-instant` returning a
`pollable`. We cannot reuse this — pollables are WASI's wait primitive,
whereas the sandbox's WAIT/resume machinery uses Component Model waitable
sets of CM subtasks. We need a CM-async import to get a waitable compatible
with the existing `_await_waitable` helper.

### 2. Host (eryx crate) implementation

```rust
async fn sleep_until(deadline_ns: u64) {
    let now_ns = wasi_monotonic_now_ns(); // same function WASI uses
    if deadline_ns <= now_ns {
        return; // past deadline; complete immediately
    }
    let delay = Duration::from_nanos(deadline_ns - now_ns);
    tokio::time::sleep(delay).await;
}
```

`wasi_monotonic_now_ns()` should use the same source wasmtime uses for
WASI monotonic-clock — the store's `initial: Instant` captured at creation,
returning `initial.elapsed().as_nanos() as u64`. Depending on how the
sandbox store is structured, this may mean reading `initial` from the store
context, or re-reading it via the WASI clock table. Verify that the value
matches what `time.monotonic()` would see from inside the guest for the
same moment; a one-off assertion test ("sleep 0 ns when deadline == guest's
now") catches any epoch mismatch.

The host-side wiring mirrors the TCP path — look at how `eryx:net/tcp` is
hooked up in `crates/eryx/src/wasm.rs` (sandbox builder + async host-fn
registration) and mirror it for timer.

### 3. Guest (eryx-wasm-runtime) plumbing

In `crates/eryx-wasm-runtime/src/lib.rs`, add:

- `call_timer_sleep_until(deadline_ns: u64) -> Result<NetResult<()>, String>`
  following the template at `lib.rs:865` (TCP connect).
- `do_timer_sleep_until(deadline_ns: u64) -> Result<(i32, NetResultValue), String>`
  following the `do_tcp_connect` template at `lib.rs:1234`.

In `crates/eryx-wasm-runtime/src/python.rs`:

- Add the PyO3 wrapper `_eryx_timer_sleep_until`, register it in `eryx_module`
  around python.rs:904.
- Inside `ERYX_ASYNC_INJECT_CODE`, add:

  ```python
  async def await_sleep_until(deadline_ns: int) -> None:
      result_type, value = _eryx._eryx_timer_sleep_until(deadline_ns)
      if result_type == 0:
          return                       # past deadline; completed immediately
      elif result_type == 1:
          raise RuntimeError(value)    # shouldn't happen in practice
      else:
          waitable, _promise = value
          await _await_waitable(waitable)
  ```

  (`_await_waitable` is the helper from Step 1.)

### 4. `_EryxLoop.call_at` / `call_later`

`_EryxLoop.time()` stays as its default `time.monotonic()` (already defined
at python.rs:1041). `time.monotonic()` returns float seconds; the WASI
instant in nanoseconds is `int(t * 1e9)`. The two agree because
`time.monotonic()` on WASI is backed by `monotonic-clock.now()` divided by
1e9.

Implement the scheduling methods:

```python
def call_at(self, when, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.TimerHandle(when, callback, args, self, context)

    async def _run_timer():
        deadline_ns = int(when * 1e9)
        await await_sleep_until(deadline_ns)
        if not handle._cancelled:
            handle._run()

    task = asyncio.Task(_run_timer(), loop=self, context=context)
    # Hook handle.cancel() to also cancel the task so the sleep subtask drops.
    original_cancel = handle.cancel
    def cancel():
        original_cancel()
        task.cancel()
    handle.cancel = cancel
    return handle

def call_later(self, delay, callback, *args, context=None):
    if delay <= 0:
        return self.call_soon(callback, *args, context=context)
    return self.call_at(self.time() + delay, callback, *args, context=context)
```

Fast path: `call_later(0, ...)` skips the sleep roundtrip entirely. Past
deadlines passed to `call_at` will short-circuit on the host side (the
`sleep-until` impl returns immediately if `deadline_ns <= now_ns`).

### 5. Remove `call_at` / `call_later` / `time` from the stub list

Currently `_EryxLoop` implicitly raises `NotImplementedError` for these. Make
sure the new implementations replace any stubs and that nothing in the class
still raises.

## Files to touch

- `crates/eryx-runtime/wit/runtime.wit` — import the new interface.
- `crates/eryx-runtime/wit/deps/timer/timer.wit` — new file with the interface
  definition.
- `crates/eryx/src/wasm.rs` (and possibly `sandbox.rs`) — register the host
  implementation following the TCP pattern.
- `crates/eryx-wasm-runtime/src/lib.rs` — guest-side import calls.
- `crates/eryx-wasm-runtime/src/python.rs`:
  - `eryx_module` at line ~904 — register PyO3 wrappers.
  - `ERYX_ASYNC_INJECT_CODE` — add `await_sleep_until`, update `_EryxLoop` with
    `time()`, `call_at()`, `call_later()`.

## How to test

Place tests in `crates/eryx/tests/` and an example in `crates/eryx/examples/`
(something like `timers.rs`).

1. **Bare sleep**: `await asyncio.sleep(0.05)`. Assert it blocked for ~50ms on
   the wall clock (with generous tolerance). Primary smoke test.

2. **Zero delay**: `await asyncio.sleep(0)` — should not round-trip through the
   host. Easy to check by ensuring `_eryx_timer_sleep_until` isn't called
   (instrument or rely on fast path).

3. **Past deadline**: `loop.call_at(loop.time() - 1, cb)` — `cb` fires on the
   next poll iteration.

4. **`asyncio.wait_for` timeout**: `wait_for(slow_callback(), timeout=0.01)`
   raises `TimeoutError`. Combined with Step 1, no subtask leak.

5. **Nested timers**: `call_later(0.01, lambda: loop.call_later(0.01, done))` —
   both fire, final callback runs.

6. **Many concurrent timers**: 100 `asyncio.sleep`s in a `gather`. All
   complete within reasonable wall-clock bounds. Not a benchmark — just sanity.

7. **Clock coherence**: `asyncio.sleep(0.1)` actually takes ~100ms, not 10ms or
   1s. This fails loudly if `time()` and `sleep-until` disagree on units or
   epoch.

8. **Timer cancelled on main exit** (exercises Step 2):
   ```python
   asyncio.create_task(asyncio.sleep(10))
   # main returns
   ```
   Sandbox shuts down promptly, no trap, no 10-second hang.

9. **TimerHandle.cancel() before firing**: schedule with `call_later`, cancel
   immediately, advance time past the deadline — callback does not fire, no
   leaked subtask.

Run via `mise run test`.

## Things to verify

- **Clock source agreement.** The host impl of `sleep-until` must use the
  same monotonic-clock epoch as WASI's `monotonic-clock.now()` (i.e., the
  store's `initial: Instant`). If the host reads a different `Instant`
  start, deadlines will drift by the offset between the two. Add a direct
  assertion test: from the guest, call `sleep-until(time.monotonic_ns())`
  and confirm it returns immediately (past deadline path) — if it instead
  blocks, the epochs disagree. Also: `asyncio.sleep(0.1)` should block
  ~100ms wall-clock.

- **Cache/build dance.** Per `AGENTS.md:117-124`, adding a WIT interface
  invalidates the late-linking artifact cache. After the WIT edit, reviewers
  (and CI) need a fresh build:
  ```
  mise run clean-late-linking-cache
  mise run setup
  ```
  Call this out explicitly in the PR description so reviewers don't hit
  phantom `type-checking export func` errors. Consider also documenting in
  `AGENTS.md` under "Symptoms of Stale Caches."

- **Preinit snapshot.** `crates/eryx-runtime/runtime.cwasm` is committed and
  embedded via `include_bytes!`. It must be rebuilt against the new WIT and
  re-committed — otherwise CI will have a stale snapshot.

- **`_eryx_reserved` name collisions.** python.rs:3208 lists reserved names
  injected into user globals. If the feature exposes anything at the
  user-globals level (probably not — `call_later` is on `loop`, not globals),
  add it here.

- **`asyncio.sleep(0)` behaviour matches CPython.** CPython treats it as
  "yield to the loop once" rather than a real timer. Confirm our fast path
  does the same.

- **`TimerHandle` interface compatibility.** CPython returns
  `asyncio.TimerHandle` with `when()`, `cancel()`, `cancelled()`. Use
  `asyncio.TimerHandle` directly rather than rolling our own, so user code
  that introspects the return value just works.

## Explicitly NOT in scope

- Don't touch `_AsyncState` / ContextVar plumbing.
- Don't implement other `AbstractEventLoop` stubs (`add_reader`,
  `run_in_executor`, etc.).
- Don't migrate to "Option B" (Python-side timer heap with rearmable single
  timer waitable). The WIT interface is forward-compatible — Option B would
  reuse `sleep-until` with a different guest-side scheduler — so nothing here
  paints us into a corner.
- Don't alter snapshot/restore behaviour for pending timers. Briefly audit
  `snapshot-state` to confirm it doesn't try to pickle loop state that now
  includes timer handles; if it does, timers should simply not survive
  snapshots (document and move on).
