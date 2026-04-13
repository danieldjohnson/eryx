//! Integration tests for the `_await_waitable` cleanup helper.
//!
//! These tests exercise the path where an asyncio Task is cancelled while
//! suspended inside a host-waitable wrapper (callback invoke, TCP, TLS). The
//! helper must pop the future from `_AsyncState.futures` and drop the host
//! subtask; otherwise the subtask leaks and `waitable_set.drop` traps at
//! shutdown when the set is still non-empty.
//!
//! The tests are intentionally in-Python: we can't drive cancellation via
//! `asyncio.wait_for` (that requires `call_later`, which is Step 3), so we
//! instead use explicit `Task.cancel()` and `await asyncio.sleep(0)` to yield
//! control back to the event loop.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
#[cfg(not(feature = "embedded"))]
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use eryx::{CallbackError, JsonSchema, Sandbox, TypedCallback};
use serde::Deserialize;
use serde_json::{Value, json};

// =============================================================================
// Test callbacks
// =============================================================================

#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    /// Milliseconds to sleep before returning.
    ms: u64,
}

/// Callback that sleeps host-side for the requested duration, giving the guest
/// a genuine pending subtask to suspend on.
struct SleepCallback;

impl TypedCallback for SleepCallback {
    type Args = SleepArgs;
    fn name(&self) -> &str {
        "sleep_cb"
    }
    fn description(&self) -> &str {
        "Sleeps host-side for the given number of milliseconds"
    }
    fn invoke_typed(
        &self,
        args: SleepArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(args.ms)).await;
            Ok(json!({"slept_ms": args.ms}))
        })
    }
}

#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    message: String,
}

struct EchoCallback;

impl TypedCallback for EchoCallback {
    type Args = EchoArgs;
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes the message back immediately"
    }
    fn invoke_typed(
        &self,
        args: EchoArgs,
    ) -> Pin<Box<dyn Future<Output = Result<Value, CallbackError>> + Send + '_>> {
        Box::pin(async move { Ok(json!({"echoed": args.message})) })
    }
}

// =============================================================================
// Helpers
// =============================================================================

#[cfg(not(feature = "embedded"))]
fn runtime_wasm_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-runtime")
        .join("runtime.wasm")
}

#[cfg(not(feature = "embedded"))]
fn python_stdlib_path() -> PathBuf {
    if let Ok(path) = std::env::var("ERYX_PYTHON_STDLIB") {
        let path = PathBuf::from(path);
        if path.exists() {
            return path;
        }
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("eryx-wasm-runtime")
        .join("tests")
        .join("python-stdlib")
}

fn sandbox_builder() -> eryx::SandboxBuilder<eryx::state::Has, eryx::state::Has> {
    #[cfg(feature = "embedded")]
    {
        Sandbox::embedded()
    }
    #[cfg(not(feature = "embedded"))]
    {
        let stdlib_path = python_stdlib_path();
        Sandbox::builder()
            .with_wasm_file(runtime_wasm_path())
            .with_python_stdlib(&stdlib_path)
    }
}

// =============================================================================
// Tests
// =============================================================================

/// Cancel an asyncio Task that is suspended awaiting a slow callback, then
/// verify the sandbox is still usable. Before the `_await_waitable` fix, the
/// subtask would leak: `waitable_set.drop` on shutdown would trap because the
/// set still contained the cancelled-but-not-dropped subtask.
#[tokio::test]
async fn test_cancel_awaiting_callback_releases_subtask() {
    let sandbox = sandbox_builder()
        .with_callback(SleepCallback)
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio

# Start a slow callback in the background.
task = asyncio.create_task(sleep_cb(ms=500))

# Yield control so the task reaches `await future` inside _await_waitable.
# A bare `await asyncio.sleep(0)` is a special case in CPython that yields
# once without scheduling a timer, so it does not depend on call_later.
for _ in range(3):
    await asyncio.sleep(0)

# Cancel while the task is suspended on the host subtask.
task.cancel()
try:
    await task
except asyncio.CancelledError:
    print("CANCELLED_OK")

# If the subtask was properly dropped, the sandbox should still be usable:
# another callback invoke goes through _await_waitable cleanly, and shutdown
# drops the waitable_set without trapping.
result = await echo(message="after_cancel")
print(f"FOLLOWUP:{result['echoed']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Sandbox should shut down cleanly after cancellation: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CANCELLED_OK"),
        "Task cancellation should surface as CancelledError: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("FOLLOWUP:after_cancel"),
        "Follow-up callback should still work after cancel: {}",
        output.stdout
    );
}

/// Cancel several in-flight callbacks back-to-back and confirm none of them
/// leak a subtask into the waitable set. This is the stress version of the
/// single-cancel test above — if cleanup is flaky, running it many times
/// surfaces the race.
#[tokio::test]
async fn test_repeated_cancellation_no_leak() {
    let sandbox = sandbox_builder()
        .with_callback(SleepCallback)
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio

cancelled = 0
for i in range(5):
    task = asyncio.create_task(sleep_cb(ms=200))
    for _ in range(3):
        await asyncio.sleep(0)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        cancelled += 1

result = await echo(message="done")
print(f"CANCELLED_COUNT:{cancelled}")
print(f"FINAL:{result['echoed']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Repeated cancellation should not leak subtasks: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("CANCELLED_COUNT:5"),
        "All five cancellations should surface as CancelledError: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("FINAL:done"),
        "Sandbox should still work after repeated cancels: {}",
        output.stdout
    );
}

/// Regression: the non-cancellation path must keep working. A plain callback
/// invoke goes through `_await_waitable` on its success branch — confirm
/// results are still delivered correctly.
#[tokio::test]
async fn test_non_cancelled_callback_still_works() {
    let sandbox = sandbox_builder()
        .with_callback(SleepCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
result = await sleep_cb(ms=10)
print(f"SLEPT:{result['slept_ms']}")
"#,
        )
        .await;

    assert!(result.is_ok(), "Happy path must keep working: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SLEPT:10"),
        "Non-cancelled callback should return its result: {}",
        output.stdout
    );
}
