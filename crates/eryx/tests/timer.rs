//! Integration tests for the timer (Step 3): `call_at`, `call_later`, and
//! `asyncio.sleep()`.
//!
//! These tests verify that:
//! - `asyncio.sleep()` works and doesn't block other coroutines
//! - `call_later` / `call_at` schedule callbacks correctly
//! - Timer cancellation releases the host subtask (via Step 1)
//! - Pending timers at shutdown are drained cleanly (via Step 2)
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

#[derive(Deserialize, JsonSchema)]
struct SleepArgs {
    ms: u64,
}

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

/// Basic smoke test: `asyncio.sleep(0.05)` should block for ~50ms.
#[tokio::test]
async fn test_asyncio_sleep_basic() {
    let sandbox = sandbox_builder()
        .build()
        .expect("Failed to build sandbox");

    let start = std::time::Instant::now();
    let result = sandbox
        .execute(
            r#"
import asyncio
await asyncio.sleep(0.05)
print("SLEPT_OK")
"#,
        )
        .await;

    let elapsed = start.elapsed();
    assert!(result.is_ok(), "asyncio.sleep should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("SLEPT_OK"),
        "Should have completed: {}",
        output.stdout
    );
    assert!(
        elapsed >= Duration::from_millis(40),
        "Should have slept for ~50ms, but only took {:?}",
        elapsed
    );
}

/// `asyncio.sleep(0)` should complete immediately without a host roundtrip.
#[tokio::test]
async fn test_asyncio_sleep_zero() {
    let sandbox = sandbox_builder()
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio
await asyncio.sleep(0)
print("ZERO_OK")
"#,
        )
        .await;

    assert!(result.is_ok(), "sleep(0) should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("ZERO_OK"),
        "Should have completed: {}",
        output.stdout
    );
}

/// Concurrent asyncio.sleep calls via gather should run in parallel,
/// not sequentially. Total time should be ~max(sleeps), not sum(sleeps).
#[tokio::test]
async fn test_concurrent_sleeps_with_gather() {
    let sandbox = sandbox_builder()
        .build()
        .expect("Failed to build sandbox");

    let start = std::time::Instant::now();
    let result = sandbox
        .execute(
            r#"
import asyncio

async def sleep_and_report(label, seconds):
    await asyncio.sleep(seconds)
    return label

results = await asyncio.gather(
    sleep_and_report("a", 0.05),
    sleep_and_report("b", 0.05),
    sleep_and_report("c", 0.05),
)
print(f"RESULTS:{','.join(results)}")
"#,
        )
        .await;

    let elapsed = start.elapsed();
    assert!(
        result.is_ok(),
        "Concurrent sleeps should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("RESULTS:a,b,c"),
        "All three sleeps should complete: {}",
        output.stdout
    );
    // If they ran in parallel, total time should be ~50ms, not ~150ms.
    assert!(
        elapsed < Duration::from_millis(200),
        "Concurrent sleeps should be parallel (~50ms), took {:?}",
        elapsed
    );
}

/// `asyncio.wait_for` with a timeout that fires should raise TimeoutError.
#[tokio::test]
async fn test_wait_for_timeout() {
    let sandbox = sandbox_builder()
        .with_callback(SleepCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio

try:
    await asyncio.wait_for(sleep_cb(ms=5000), timeout=0.05)
    print("NO_TIMEOUT")
except asyncio.TimeoutError:
    print("TIMEOUT_OK")
"#,
        )
        .await;

    assert!(result.is_ok(), "wait_for should work: {:?}", result);
    let output = result.unwrap();
    assert!(
        output.stdout.contains("TIMEOUT_OK"),
        "Should have timed out: {}",
        output.stdout
    );
}

/// Timer cancelled on main exit (exercises Step 2 drain):
/// `asyncio.create_task(asyncio.sleep(10))` then main returns immediately.
/// Should shut down promptly, no trap, no 10-second hang.
#[tokio::test]
async fn test_timer_drained_on_shutdown() {
    let sandbox = sandbox_builder()
        .build()
        .expect("Failed to build sandbox");

    let start = std::time::Instant::now();
    let result = sandbox
        .execute(
            r#"
import asyncio
asyncio.create_task(asyncio.sleep(10))
# Yield so the timer task starts and reaches the host sleep.
await asyncio.sleep(0)
print("MAIN_DONE")
"#,
        )
        .await;

    let elapsed = start.elapsed();
    assert!(
        result.is_ok(),
        "Pending timer should be drained cleanly: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("MAIN_DONE"),
        "Main should complete: {}",
        output.stdout
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "Should not hang for 10s, took {:?}",
        elapsed
    );
}

/// Callback + sleep interleaved: a slow callback and a sleep should both
/// work in the same execution.
#[tokio::test]
async fn test_sleep_with_callback() {
    let sandbox = sandbox_builder()
        .with_callback(EchoCallback)
        .build()
        .expect("Failed to build sandbox");

    let result = sandbox
        .execute(
            r#"
import asyncio
await asyncio.sleep(0.01)
result = await echo(message="after_sleep")
print(f"RESULT:{result['echoed']}")
"#,
        )
        .await;

    assert!(
        result.is_ok(),
        "Sleep + callback should work: {:?}",
        result
    );
    let output = result.unwrap();
    assert!(
        output.stdout.contains("RESULT:after_sleep"),
        "Callback after sleep should return result: {}",
        output.stdout
    );
}
