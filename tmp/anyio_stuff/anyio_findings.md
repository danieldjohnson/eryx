# anyio in eryx: findings

## Summary

anyio task groups work inside eryx, but eryx's minimal event loop needs patching. Two shims
are required: the existing **socket shim** (for import compatibility) and a new **loop shim**
(for runtime compatibility).

All of the loop shim fixes are candidates for upstreaming to eryx. The `call_soon`,
`get_task_factory`, and `time` fixes are trivial one-liners. The `call_later`/`call_at` fix
is more involved but fits cleanly into eryx's existing architecture.

## Socket shim (existing)

Eryx's WASM `socket` module is missing `AddressFamily`, `SocketKind`, and `AF_UNSPEC`. anyio
needs these at import time. The existing shim in `runner.py` patches them in as `IntEnum`
wrappers over the raw constants that eryx does provide.

## Eryx's event loop architecture

Understanding why the fixes work requires understanding eryx's event loop design.

Eryx has a **singleton** `_EryxLoop` (created once at module load). But per-execution state
lives in `_AsyncState`, a dataclass holding the callback handle queue, pending futures, and
a pending count. Each `execute()` call creates exactly one `_AsyncState` via `run_async()`:

```
execute_python(code)                         # Rust entry point
  → _eryx_exec(code)                         # Python: compile + detect async
    → _eryx_run_async(coro)
      → run_async(coro):
          ctx = Context()                     # fresh contextvars.Context
          state = _AsyncState(handles=[], …)  # fresh per-execution state
          ctx.run(_set_async_state, state)    # bind state into context
          create_task(coro, context=ctx)
          → _poll(state)                      # drain handles, return WAIT or EXIT
```

The `_AsyncState` is bound to a `Context` via a `ContextVar`. The loop's `call_soon` uses
`context[_async_state].handles.append(handle)` to route callbacks to the right state. This
isn't for multiplexing concurrent executions — there's only ever one active `_AsyncState` at
a time. It's for **isolation between sequential `execute()` calls**, so handles from one
execution don't leak into the next.

The execution model is cooperative and single-threaded:
1. `poll()` drains all ready handles
2. If nothing is pending → `EXIT`
3. If a callback is needed → `WAIT` (Rust host handles the callback)
4. Rust calls `resume()` → back to `poll()`
5. Repeat until `EXIT`

## Loop shim fixes

### `get_task_factory()` → returns `None`

anyio's `TaskGroup._spawn()` calls `loop.get_task_factory()` to check for an eager task
factory (Python 3.12+ API). Eryx raises `NotImplementedError`, which kills task spawning.
Returning `None` makes anyio fall back to `loop.create_task()`, which eryx supports.

**Upstream fix:** One line — `def get_task_factory(self): return None`.

### `call_soon()` → context fallback

anyio's cancel scope delivery calls `loop.call_soon(callback, *args)` without passing
`context`. Eryx asserts context is non-None because it uses `context[_async_state]` to find
the handle queue.

Since there's only one `_AsyncState` per execution, `copy_context()` always captures the
right state. The fix is safe.

**Upstream fix:** Replace the assertion with a fallback:
```python
def call_soon(self, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.Handle(callback, args, self, context)
    context[_async_state].handles.append(handle)
    return handle
```

This matches CPython's own event loop behavior (uses `copy_context()` when no context is
provided).

### `time()` → `time.monotonic()`

anyio uses `loop.time()` for cancel scope deadlines. Eryx raises `NotImplementedError`.

**Upstream fix:** `def time(self): return time.monotonic()`. Same as what CPython's event
loop does.

### `call_later()` / `call_at()` — needs real implementation

Currently shimmed as immediate execution (collapse delay to 0). This breaks:
- `anyio.sleep(n)` — returns instantly
- Timed cancellation — sleep completes before spawned tasks start
- Cancel scope deadlines / connection timeouts

**This needs a proper fix, not a shim.** See "Proposed `call_later` design" below.

## What works (with current shims)

- `anyio.create_task_group()` with `start_soon()` and concurrent tasks
- `anyio.create_memory_object_stream()` — send, receive, iteration, close
- Producer/consumer patterns across tasks in a task group
- `cancel_scope.cancel()` — scopes do cancel
- `anyio.sleep()` — returns but with no actual delay (see above)

## What doesn't work

- Real delays / `anyio.sleep()` with actual waiting
- Timed cancel scopes (deadlines)
- Any code relying on sleep to yield and let other tasks interleave

## Proposed `call_later` design

The cleanest approach is to add a timer heap to `_AsyncState` and integrate it into the
existing `poll → WAIT → resume` cycle.

### Guest-side changes (Python, in `_EryxLoop`)

Add a timer heap to `_AsyncState`:
```python
@dataclass
class _AsyncState:
    waitable_set: Optional[int]
    futures: dict[int, asyncio.Future]
    handles: list[asyncio.Handle]
    pending_count: int
    timer_heap: list[tuple[float, int, asyncio.Handle]]  # (deadline, tiebreaker, handle)
    timer_counter: int  # monotonic tiebreaker for heap stability
```

Implement `call_later` and `call_at`:
```python
def call_later(self, delay, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    return self.call_at(self.time() + delay, callback, *args, context=context)

def call_at(self, when, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.Handle(callback, args, self, context)
    state = context[_async_state]
    heapq.heappush(state.timer_heap, (when, state.timer_counter, handle))
    state.timer_counter += 1
    return handle

def time(self):
    return time.monotonic()
```

Update `poll()` to fire ready timers:
```python
def poll(self, state):
    while True:
        # Fire ready timers
        now = time.monotonic()
        while state.timer_heap and state.timer_heap[0][0] <= now:
            _, _, handle = heapq.heappop(state.timer_heap)
            if not handle._cancelled:
                handle._run()
        # Drain immediate handles (existing logic)
        handles, state.handles = state.handles, []
        for h in handles:
            if not h._cancelled:
                h._run()
        if self.exception is not None:
            raise self.exception
        if not handles and not state.handles:
            return
```

### Host-side changes (Rust + `_poll`)

`_poll` needs a new return path: "no callbacks pending, but timers are scheduled."

```python
# New callback code
WAIT_TIMER = 3

def _poll(state):
    _loop.poll(state)
    if state.pending_count == 0:
        if state.waitable_set is not None:
            _eryx.waitable_set_drop_(state.waitable_set)
        return EXIT
    elif state.timer_heap:
        next_deadline = state.timer_heap[0][0]
        delay_ms = max(0, int((next_deadline - time.monotonic()) * 1000))
        return WAIT_TIMER | (delay_ms << 4)
    else:
        assert state.waitable_set is not None, "pending but no waitable_set"
        _eryx.context_set_(state)
        return WAIT | (state.waitable_set << 4)
```

On the Rust side, when `_poll` returns `WAIT_TIMER`, the host sleeps for `delay_ms`
milliseconds, then calls `resume()` with a timer event (or just `_EVENT_NONE` since
`poll()` will fire the ready timers itself).

This approach:
- Fits naturally into eryx's existing `poll → return status → host acts → resume` model
- Doesn't change any existing behavior (no timers = same as before)
- The Python changes are ~20 lines, the Rust changes are small (new match arm in the
  poll loop)
- All timer logic is in Python; Rust just needs to sleep and re-enter

### Open question: timers + callbacks

What happens when both timers and WASM callbacks are pending? The host needs to either:
- (a) Wait for whichever comes first (callback completion OR timer deadline)
- (b) Always prioritize callbacks over timers

Option (a) is more correct but requires the Rust poll loop to do a select/timeout on the
waitable set. Option (b) is simpler — timers only fire when there's nothing else to do.
For our use case (RPC bridge where real work goes through callbacks), (b) is probably fine.

## Current shim code

The current shim (from `step2_anyio_task_groups.py`). This is temporary — the `call_later`
and `call_at` implementations here collapse delays and need to be replaced with the proper
timer heap approach (either as an upstream fix or a more complete guest-side shim).

```python
import asyncio as _asyncio
import contextvars as _contextvars
import time as _time

_loop = _asyncio.get_event_loop()

_loop.get_task_factory = lambda: None

_orig_call_soon = _loop.call_soon
def _patched_call_soon(callback, *args, context=None):
    if context is None:
        context = _contextvars.copy_context()
    return _orig_call_soon(callback, *args, context=context)
_loop.call_soon = _patched_call_soon

_start_time = _time.monotonic()
_loop.time = lambda: _time.monotonic() - _start_time

def _call_later(delay, callback, *args, context=None):
    return _patched_call_soon(callback, *args, context=context)
_loop.call_later = _call_later

def _call_at(when, callback, *args, context=None):
    return _patched_call_soon(callback, *args, context=context)
_loop.call_at = _call_at
```
