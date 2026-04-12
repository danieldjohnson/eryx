# Plan: anyio Support for Eryx WASM Sandbox

## Context

anyio is needed inside eryx's WASM sandbox for an RPC bridge that uses task groups, memory streams, and concurrent async patterns. Eryx's `_EryxLoop` is minimal — it's missing several methods anyio requires. This plan adds them in two phases: simple patches first, then real timer support.

## Phase 1: Event Loop Patches (Python-only)

All changes are to embedded Python string literals in `crates/eryx-wasm-runtime/src/python.rs`. No WIT, host Rust, or Cargo changes.

### Step 1.1: Add imports to `_eryx_async` module (~line 960)

Add `import contextvars` (full module, alongside existing `from contextvars import ...`) and `import time` at the top of the embedded async module.

### Step 1.2: Fix `call_soon` context handling (~line 1010)

Replace `raise AssertionError("context required for call_soon")` with `context = contextvars.copy_context()`. This matches CPython's behavior and is safe because there's only one `_AsyncState` per execution.

### Step 1.3: Implement `time()` (~line 1036)

Replace `raise NotImplementedError` with `return time.monotonic()`. Same as CPython's event loop.

### Step 1.4: Add `get_task_factory()` to `_EryxLoop` (after line 1033)

```python
def get_task_factory(self):
    return None
```

anyio calls this to check for eager task factory. `None` = use `loop.create_task()`.

### Step 1.5: Socket shim additions (~line 1937)

Add `AF_UNSPEC = 0` and `AddressFamily`/`SocketKind` IntEnum classes after existing socket constants. anyio imports these at module load time.

### Phase 1 Testing

Rebuild WASM runtime and test with anyio task groups, memory streams, and explicit cancellation (no real timer delays needed). Adapt the existing test script in `tmp/anyio_stuff/step2_anyio_task_groups.py` — the socket shim and most of the loop shim should no longer be needed.

---

## Phase 2: Timer Support via Component Model Async Sleep

### Design Decision: Why Component Model Async (not fiber-based)

Eryx has two async mechanisms:
- **Fiber-based** (TCP/TLS): Blocks the entire guest during the call. Simple but wrong for sleep — no other Python coroutines can progress.
- **Component Model async** (`invoke`): Creates a waitable/subtask. Guest can have multiple operations in flight. This is what sleep needs.

### Design Decision: Async Task Per Timer (not timer heap)

Instead of a timer heap managed by `_poll()`, each `call_later(delay, cb)` creates an asyncio Task that awaits a Component Model async `sleep` import. Each timer becomes a waitable in the shared waitable set. The Component Model runtime multiplexes them automatically.

**Why this design works for the callback-during-sleep case**: If a callback returns while a sleep is pending, both waitables are in the same set. The Component Model wakes the guest for whichever fires first. The callback handler can schedule new timers (creating new sleep waitables). Old sleep waitables stay in the set and fire independently. No explicit cancellation or rescheduling needed.

### Step 2.1: WIT — Add `sleep` import

**Files**: `crates/eryx-runtime/wit/runtime.wit` AND `crates/eryx/wit/runtime.wit` (identical copies, both must be updated)

```wit
import sleep: async func(milliseconds: u64);
```

`async func` means wasmtime uses Component Model async — same mechanism as `invoke`.

### Step 2.2: Host — Implement `sleep`

**File**: `crates/eryx/src/wasm.rs`

Add to `impl SandboxImportsWithStore for HasSelf<ExecutorState>` (after `invoke`, ~line 497):

```rust
fn sleep<T>(
    _accessor: &Accessor<T, Self>,
    milliseconds: u64,
) -> impl ::core::future::Future<Output = ()> + Send {
    async move {
        tokio::time::sleep(std::time::Duration::from_millis(milliseconds)).await;
    }
}
```

No channels needed — unlike `invoke` which routes to a callback handler, sleep is self-contained.

### Step 2.3: Guest Rust — `ImportType::Sleep`

**File**: `crates/eryx-wasm-runtime/src/lib.rs` (~line 601)

Add `Sleep` variant to `ImportType` enum.

### Step 2.4: Guest Rust — `call_sleep_async` function

**File**: `crates/eryx-wasm-runtime/src/lib.rs`

New function modeled on `call_invoke_async` (~line 631). Differences:
- Single `u64` argument (pushed as `Value::U64`)
- Gets import via `wit.get_import(None, "sleep")`
- Returns `SleepResult::Complete` or `SleepResult::Pending(subtask, 0)`
- On immediate completion: no result to pop (sleep returns unit)

### Step 2.5: Guest Rust — `_eryx_sleep_async` Python function

**File**: `crates/eryx-wasm-runtime/src/python.rs`

New `#[pyfunction]` similar to `_eryx_invoke_async` (~line 572). Returns `(result_type, value)` tuple: 0=complete, 1=error, 2=pending with `(waitable, promise)`.

Register in `eryx_module` (~line 906).

### Step 2.6: Guest Rust — Handle sleep in `export_async_callback`

**File**: `crates/eryx-wasm-runtime/src/lib.rs` (~line 1605)

In the `match pending_state.import_type` block, add:
```rust
ImportType::Sleep => {
    // Sleep returns unit — no result to extract or store.
    // Python's resume() resolves the future via set_result().
}
```

The lift function is still called (unconditionally before the match), but for unit-return it pushes nothing meaningful. The match arm is a no-op.

### Step 2.7: Guest Python — Timer methods and helpers

**File**: `crates/eryx-wasm-runtime/src/python.rs` (embedded Python)

Add to `_EryxLoop` class:

```python
def call_later(self, delay, callback, *args, context=None):
    if context is None:
        context = contextvars.copy_context()
    handle = asyncio.Handle(callback, args, self, context)
    if delay <= 0:
        context[_async_state].handles.append(handle)
    else:
        state = context[_async_state]
        state.pending_count += 1
        asyncio.create_task(
            _delayed_callback(delay, handle, state),
            context=context,
        )
    return handle

def call_at(self, when, callback, *args, context=None):
    delay = max(0, when - self.time())
    return self.call_later(delay, callback, *args, context=context)
```

Add module-level helpers:

```python
async def _delayed_callback(delay_secs, handle, state):
    try:
        ms = max(1, int(delay_secs * 1000))
        await _await_sleep(ms)
        if not handle._cancelled:
            handle._run()
    finally:
        state.pending_count -= 1

async def _await_sleep(milliseconds):
    result_type, value = _eryx._eryx_sleep_async(milliseconds)
    if result_type == 0:
        return
    elif result_type == 1:
        raise RuntimeError(value)
    else:
        waitable, _promise = value
        future = _loop.create_future()
        state = _async_state.get()
        state.futures[waitable] = future
        if state.waitable_set is None:
            state.waitable_set = _eryx.waitable_set_new_()
        _eryx.waitable_join_(waitable, state.waitable_set)
        await future
```

### Key Behavior: `pending_count` Management

- `run_async` starts with `pending_count = 1` (for the main task)
- Each `call_later(delay > 0)` increments `pending_count` (prevents premature EXIT)
- `_delayed_callback`'s `finally` block always decrements it
- When main task finishes: `_return_result` decrements count
- `_poll` returns EXIT only when `pending_count == 0` — all timers must complete

### Edge Cases

| Case | Behavior |
|------|----------|
| **`delay <= 0`** | Fast path: appends directly to handles (like `call_soon`), no sleep import |
| **Cancelled handle** | Sleep import completes, `_cancelled` check skips `_run()`, `finally` decrements count |
| **Multiple concurrent timers** | Each gets its own waitable in the shared set; Component Model multiplexes them |
| **Callback returns during sleep** | Callback waitable fires first, guest resumes, processes it, old sleep fires later |
| **New timer scheduled during callback** | Creates new sleep waitable, joins same set, fires independently |
| **Exception in `handle._run()`** | asyncio.Handle._run() catches and routes to `loop.call_exception_handler` internally |
| **`_await_sleep` failure** | Exception propagates to Task, `finally` still runs, `pending_count` still decrements |

### Phase 2 Testing

Test real delays, concurrent timers, and cancel scope deadlines:
```python
# Verify wall-clock timing
await anyio.sleep(0.5)  # should take ~0.5s

# Verify concurrent timer multiplexing
async with anyio.create_task_group() as tg:
    tg.start_soon(anyio.sleep, 0.3)
    tg.start_soon(anyio.sleep, 0.5)
# Should take ~0.5s total (not 0.8s)

# Verify timed cancellation
with anyio.move_on_after(0.2):
    await anyio.sleep(10.0)  # cancelled after ~0.2s
```

---

## Files Modified

| File | Phase | Changes |
|------|-------|---------|
| `crates/eryx-wasm-runtime/src/python.rs` | 1 + 2 | Embedded Python: imports, event loop methods, `_eryx_sleep_async` pyfunction |
| `crates/eryx-runtime/wit/runtime.wit` | 2 | Add `sleep` async import |
| `crates/eryx/wit/runtime.wit` | 2 | Add `sleep` async import (must match) |
| `crates/eryx-wasm-runtime/src/lib.rs` | 2 | `ImportType::Sleep`, `call_sleep_async`, `export_async_callback` match arm |
| `crates/eryx/src/wasm.rs` | 2 | Host `sleep` implementation |

## Build & Verify

After each phase:
```bash
mise run build-eryx-runtime
mise run precompile-eryx-runtime-preinit
mise run test
```
