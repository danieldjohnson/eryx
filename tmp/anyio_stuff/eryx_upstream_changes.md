# Proposed eryx upstream changes

Changes to eryx needed for anyio compatibility. Grouped by file and ordered by complexity.

## 1. Socket module: add missing types

**File:** `crates/eryx-wasm-runtime/src/python.rs`, lines ~1923-1937 (inside `SOCKET_SHIM_CODE`)

The socket constants block currently defines `AF_INET`, `AF_INET6`, `AF_UNIX`, `SOCK_STREAM`,
`SOCK_DGRAM`, etc. but is missing three things anyio needs at import time.

**Add after the existing constants (around line 1937):**

```python
AF_UNSPEC = 0

import enum as _enum

class AddressFamily(_enum.IntEnum):
    AF_UNSPEC = 0
    AF_INET = AF_INET
    AF_INET6 = AF_INET6
    AF_UNIX = AF_UNIX

class SocketKind(_enum.IntEnum):
    SOCK_STREAM = SOCK_STREAM
    SOCK_DGRAM = SOCK_DGRAM
```

**Why:** anyio unconditionally does `from socket import AddressFamily, SocketKind` and uses
`AF_UNSPEC` as a default argument value. Without these, `import anyio` fails with
`ImportError: cannot import name 'AddressFamily' from 'socket'`.

## 2. Event loop: `get_task_factory()`

**File:** `crates/eryx-wasm-runtime/src/python.rs`, inside the `_EryxLoop` class (line ~986)

**Add method:**

```python
def get_task_factory(self):
    return None
```

**Where:** Alongside the other stub methods, around line 1036.

**Why:** anyio 4.x calls `loop.get_task_factory()` (Python 3.12+ API) to check for an eager
task factory. Eryx inherits `AbstractEventLoop.get_task_factory()` which raises
`NotImplementedError`. Returning `None` tells anyio to use `loop.create_task()` instead,
which eryx supports.

## 3. Event loop: `call_soon()` context fallback

**File:** `crates/eryx-wasm-runtime/src/python.rs`, `_EryxLoop.call_soon()` (line ~1010)

**Current code:**

```python
def call_soon(self, callback, *args, context=None):
    if context is None:
        raise AssertionError("context required for call_soon")
    handle = asyncio.Handle(callback, args, self, context)
    context[_async_state].handles.append(handle)
    return handle
```

**Proposed change:**

```python
def call_soon(self, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.Handle(callback, args, self, context)
    context[_async_state].handles.append(handle)
    return handle
```

(Requires adding `import contextvars` at the top of the `_eryx_async` module if not already
there — `contextvars.ContextVar` and `contextvars.Context` are already imported on line 962,
but as `from contextvars import ContextVar, Context`. Changing to `import contextvars` or
adding it alongside would work.)

**Why:** anyio's cancel scope delivery calls `loop.call_soon(callback, *args)` without a
`context` kwarg. CPython's own event loop defaults to `copy_context()` in this case. The
assertion was originally a safety check since eryx routes handles via
`context[_async_state].handles`, but since there's only ever one `_AsyncState` per execution,
`copy_context()` always captures the right state.

## 4. Event loop: `time()`

**File:** `crates/eryx-wasm-runtime/src/python.rs`, `_EryxLoop` stub methods (line ~1036)

**Current:** `def time(self): raise NotImplementedError`

**Change to:**

```python
def time(self):
    return time.monotonic()
```

(Requires adding `import time` at the top of the `_eryx_async` module, around line 960.)

**Why:** anyio uses `loop.time()` for cancel scope deadlines. This is the same implementation
CPython's asyncio event loop uses.

## 5. Event loop: `call_later()` / `call_at()` with timer heap

This is the largest change. It adds real delayed execution to `_EryxLoop`.

### 5a. Add timer state to `_AsyncState`

**File:** `crates/eryx-wasm-runtime/src/python.rs`, `_AsyncState` dataclass (line ~978)

```python
@dataclass
class _AsyncState:
    """State for tracking pending async operations."""
    waitable_set: Optional[int]
    futures: dict[int, asyncio.Future]
    handles: list[asyncio.Handle]
    pending_count: int
    timer_heap: list  # [(deadline, counter, handle), ...] — min-heap by deadline
    timer_counter: int  # monotonic tiebreaker for stable heap ordering
```

**Update `run_async()`** (line ~1070) to initialize the new fields:

```python
def run_async(coro) -> int:
    ctx = Context()
    state = _AsyncState(None, {}, [], 1, [], 0)  # added timer_heap=[], timer_counter=0
    ctx.run(_set_async_state, state)
    asyncio.create_task(_return_result(coro), context=ctx)
    return _poll(state)
```

### 5b. Implement `call_later` and `call_at`

**File:** `crates/eryx-wasm-runtime/src/python.rs`, inside `_EryxLoop` class

```python
def call_later(self, delay, callback, *args, context=None):
    return self.call_at(self.time() + delay, callback, *args, context=context)

def call_at(self, when, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.Handle(callback, args, self, context)
    state = context[_async_state]
    heapq.heappush(state.timer_heap, (when, state.timer_counter, handle))
    state.timer_counter += 1
    return handle
```

(Requires `import heapq` at the top of the module.)

### 5c. Update `poll()` to fire ready timers

**File:** `crates/eryx-wasm-runtime/src/python.rs`, `_EryxLoop.poll()` (line ~997)

```python
def poll(self, state):
    while True:
        # Fire ready timers
        now = time.monotonic()
        while state.timer_heap and state.timer_heap[0][0] <= now:
            _, _, handle = heapq.heappop(state.timer_heap)
            if not handle._cancelled:
                handle._run()
        # Drain immediate handles (existing logic, unchanged)
        handles, state.handles = state.handles, []
        for h in handles:
            if not h._cancelled:
                h._run()
        if self.exception is not None:
            raise self.exception
        if not handles and not state.handles:
            return
```

### 5d. New return code: `WAIT_TIMER`

**Python side** — add a new callback code (line ~967):

```python
EXIT = 0
WAIT = 2
WAIT_TIMER = 3  # new: pending timers but no WASM callbacks
```

**Update `_poll()`** (line ~1096) to return `WAIT_TIMER` when only timers are pending:

```python
def _poll(state):
    _loop.poll(state)
    if state.pending_count == 0:
        if state.waitable_set is not None:
            _eryx.waitable_set_drop_(state.waitable_set)
        return EXIT
    elif not state.timer_heap:
        # Only WASM callbacks pending (existing path)
        assert state.waitable_set is not None, "pending but no waitable_set"
        _eryx.context_set_(state)
        return WAIT | (state.waitable_set << 4)
    elif state.waitable_set is not None:
        # Both timers and WASM callbacks pending.
        # Return WAIT so the host waits for WASM callbacks. Timers will fire
        # on the next poll() after their deadline passes.
        _eryx.context_set_(state)
        return WAIT | (state.waitable_set << 4)
    else:
        # Only timers pending, no WASM callbacks.
        next_deadline = state.timer_heap[0][0]
        delay_ms = max(0, int((next_deadline - time.monotonic()) * 1000))
        _eryx.context_set_(state)
        return WAIT_TIMER | (delay_ms << 4)
```

**Rust side** — `crates/eryx-wasm-runtime/src/python.rs`, `callback_code` module (line ~225):

```rust
pub mod callback_code {
    pub const EXIT: u32 = 0;
    pub const WAIT: u32 = 2;
    pub const WAIT_TIMER: u32 = 3;  // new
    // ...
}
```

**Rust side** — `crates/eryx-wasm-runtime/src/lib.rs`, wherever `WAIT` is handled:

The key location is `execute_python()` in `python.rs` (line ~2764):

```rust
if code == callback_code::WAIT {
    return ExecuteResult::Pending(callback_code_value);
}
```

And `export_async_callback()` in `lib.rs` (line ~1634):

```rust
if code == python::callback_code::WAIT {
    return callback_code;
}
```

Both of these need a new branch for `WAIT_TIMER`. The behavior is: sleep for `delay_ms`
(extracted from the upper bits: `callback_code_value >> 4`), then call `resume()` with
`(_EVENT_NONE, 0, 0)` to re-enter `poll()` which will fire the ready timers.

The exact mechanism depends on how eryx's host runtime works. Options:
- If the host has an async runtime (tokio, etc.): `tokio::time::sleep(duration).await`
- If it's synchronous: `std::thread::sleep(duration)`
- If it's WASM component model: may need to use the component model's own timer/poll mechanism

The `ExecuteResult` enum may need a new variant:

```rust
pub enum ExecuteResult {
    Complete(ExecuteOutput),
    Error(String),
    Pending(u32),       // waiting for WASM callback
    PendingTimer(u32),  // waiting for timer — upper bits = delay_ms
}
```

Or `WAIT_TIMER` could reuse `Pending` and the caller distinguishes by the code bits.

## Summary of changes by scope

| Change | Lines of Python | Lines of Rust | Risk |
|--------|----------------|---------------|------|
| Socket types | ~12 | 0 | None |
| `get_task_factory` | 2 | 0 | None |
| `call_soon` fallback | 2 | 0 | Low |
| `time()` | 1 | 0 | None |
| `call_later`/`call_at` + timer heap | ~30 | ~20 | Medium |

Changes 1–4 are straightforward and could be a single small PR. Change 5 (timer heap) is
the only one with architectural implications and would benefit from discussion with eryx
maintainers about the right `WAIT_TIMER` mechanism for their host runtime.
