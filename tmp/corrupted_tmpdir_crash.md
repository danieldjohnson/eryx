# `SandboxFactory()` crash from corrupted stdlib cache in `$TMPDIR`

## TL;DR — quick fix

When `eryx.SandboxFactory()` (or any code that triggers Python init in the
sandbox) starts failing with an opaque `InitializationError: pre-initialization
failed`, run:

```bash
rm -rf "$TMPDIR/eryx-embedded"
```

(On macOS this expands to `/var/folders/<...>/T/eryx-embedded`. On Linux it is
typically `/tmp/eryx-embedded`.)

Then re-run. The next call will re-extract a fresh, complete copy of the
embedded Python stdlib and the crash will go away.

A reboot also fixes it because macOS clears `/var/folders/.../T/` on boot.

## Symptoms

`eryx.SandboxFactory()` fails with:

```
eryx.InitializationError: pre-initialization failed: error while executing at wasm backtrace:
    0:  ... wasi_snapshot_preview1::bindings::wasi::cli::exit::exit ...
    1:  ... proc_exit
    ...
    6:  libpython3.14.so!fatal_error_exit
    7:  libpython3.14.so!fatal_error
    8:  libpython3.14.so!Py_ExitStatusException
    9:  libpython3.14.so!Py_InitializeEx
   10:  liberyx_runtime.so!wit_dylib_initialize
```

By default, the WASI context used during pre-init does not capture stdout/stderr,
so Python's actual fatal-error message is dropped. With stdout/stderr inherited
(see "How to see Python's real error" below), the underlying message is:

```
Fatal Python error: init_fs_encoding: failed to get the Python codec of the filesystem encoding
Python runtime state: core initialized
LookupError: no codec search functions registered: can't find encoding
```

This means the embedded Python interpreter cannot import the `encodings`
package during startup.

## Root cause

The embedded Python standard library is shipped inside the eryx binary as
`crates/eryx/python-stdlib.tar.zst` and extracted to a host-side temp directory
on first use. The cache lives at a fixed, deterministic path:

```text
$TMPDIR/eryx-embedded/python-stdlib/
```

On macOS, `$TMPDIR` is `/var/folders/<user-hash>/T/`, which is stable
across processes and reboots-of-the-process but is subject to macOS's
periodic cleanup of `/var/folders/.../T/` (it removes long-untouched files
but typically leaves directory entries behind).

The relevant code is `crates/eryx/src/embedded_stdlib.rs`:

```rust
fn extract_stdlib(temp_dir: &Path) -> Result<PathBuf, Error> {
    let stdlib_path = temp_dir.join("python-stdlib");

    // Check if already extracted (quick validation: directory exists with some files)
    if stdlib_path.exists() {
        // Verify it looks valid (has encodings/ which is required for Python init)
        if stdlib_path.join("encodings").exists() {
            return Ok(stdlib_path);   // ← reuse cached extraction
        }
        let _ = std::fs::remove_dir_all(&stdlib_path);
    }
    // ... otherwise extract from the embedded tar.zst ...
}
```

The validity check only verifies that `encodings/` *exists as a directory*. It
does **not** verify that `encodings/__init__.py` is present, or that
`codecs.py` exists at the top level. If macOS's tmp cleanup has purged
individual `.py` files but left the directory tree intact, this check passes,
the broken extraction is reused, and Python fails to initialize because it
cannot import `encodings`.

This explains every observed property of the bug:

- **Reboot fixes it** — macOS wipes `/var/folders/.../T/` on boot, so the
  broken cache disappears and a fresh extraction happens on next use.
- **Affects the shipped wheel too** — same code path, same fixed cache
  directory, same lax validity check.
- **Reoccurs over time** — the macOS tmp cleanup eventually deletes files
  again from any extraction that has gone untouched for a few days.

### Confirming the diagnosis

A directory listing of the extracted stdlib in the broken state shows the
shape of the corruption:

```text
$ ls "$TMPDIR/eryx-embedded/python-stdlib/encodings/"
(empty)

$ ls "$TMPDIR/eryx-embedded/python-stdlib/codecs.py"
ls: ...: No such file or directory

$ zstd -dcf crates/eryx/python-stdlib.tar.zst | tar -t | grep -E "(codecs\.py$|encodings/__init__\.py)"
python-stdlib/codecs.py
python-stdlib/encodings/__init__.py
```

The tarball contains the files; the on-disk extraction does not. Top-level
`.py` files and `.py` files inside subdirectories are missing while
directory entries survived — the fingerprint of an mtime/atime-based cleanup.

## How to recover (user-side)

Pick whichever is easiest:

```bash
# Targeted: clear only the eryx cache.
rm -rf "$TMPDIR/eryx-embedded"

# Or: reboot. (macOS clears /var/folders/.../T/ on boot.)
```

Either way, the next invocation of `eryx.SandboxFactory()` (or any other
sandbox code that needs the stdlib) will re-extract a complete copy.

## How to see Python's real error (when debugging)

The opaque "pre-initialization failed" wraps a fatal Python error written to
WASI stderr that is normally discarded. To surface it, add stderr/stdout
inheritance to the pre-init WASI context in
`crates/eryx-runtime/src/preinit.rs`:

```rust
let mut wasi_builder = WasiCtxBuilder::new();
wasi_builder
    .inherit_stdout()      // ← add
    .inherit_stderr()      // ← add
    .env("PYTHONHOME", "/python-stdlib")
    .env("PYTHONPATH", &python_path)
    .env("PYTHONUNBUFFERED", "1");
```

Rebuild the Python extension (`cd crates/eryx-python && uv run maturin
develop --release`) and re-run the failing test with `pytest -s`. Python's
fatal-error message and path-config dump will print to your terminal,
making the underlying cause obvious.

## Suggested upstream fix

The `EmbeddedStdlib` cache validity check should be tightened. Two reasonable
options:

1. **Sentinel file.** Write a `.complete` marker into the extracted directory
   *after* a successful extraction, and require that marker to consider the
   cache valid. Cheap and robust against any kind of partial-removal.

2. **Stronger structural check.** Verify a couple of representative files
   that Python init actually needs, e.g. `encodings/__init__.py` and
   `codecs.py`, instead of just the `encodings/` directory.

A third, complementary, option is to relocate the cache out of `$TMPDIR` and
into a location macOS will not periodically purge (e.g.
`~/Library/Caches/eryx/` on macOS, `$XDG_CACHE_HOME/eryx/` on Linux). This
avoids the recurring cost of re-extraction every few days.

Either fix on its own would have made this failure self-healing instead of
sticky.
