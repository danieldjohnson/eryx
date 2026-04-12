# Container Build Environment

A Linux container for building eryx's WASM artifacts in a clean, isolated environment. This is useful when the host build breaks due to stale caches, toolchain mismatches, or corrupted artifacts.

## When to use this

The eryx build pipeline has multiple cache layers (see AGENTS.md) that can get into bad states. Symptoms include:

- `__wasi_init_tp` or other unresolved symbol errors during precompile
- `mise run setup` fails after clearing caches
- WASM artifacts built with a different WASI SDK or nightly Rust version

The container provides a clean room with pinned versions of Rust, nightly, and WASI SDK 27 to produce known-good WASM artifacts.

## What the container builds

The container builds the **WASM-level artifacts** that are platform-independent:

- `runtime.wasm` — the assembled WASM component (~34 MB)
- `prebuilt/liberyx_runtime.so.zst` — compressed WASM shared object (eryx-wasm-runtime compiled to wasm32-wasip1)
- `prebuilt/liberyx_bindings.so.zst` — compressed WIT bindings WASM shared object

These are then used on the **host (macOS)** to produce the platform-specific:

- `runtime.cwasm` — AOT-compiled native code for the host platform

The `runtime.cwasm` cannot be built inside the container for use on macOS because it contains native machine code targeting a specific OS+arch.

## Setup

### Build the image (one-time)

```bash
container build -t eryx-dev container/
```

### Start the container

```bash
container run -d --name eryx-dev \
  -v /path/to/eryx:/workspace \
  -c 8 -m 8G \
  eryx-dev:latest sleep infinity
```

The source tree is bind-mounted at `/workspace`, so artifacts written by the container are immediately visible on the host.

## Build process

> **Important — stale late-linking artifacts:** `python.rs` code is compiled into
> *both* `runtime.wasm` *and* `liberyx_runtime.so.zst`. The preinit step uses
> `liberyx_runtime.so` as the live code during Python initialization, so if that
> `.so` is stale, your `python.rs` changes will not take effect even though
> `runtime.wasm` has them. Always clear `crates/eryx-runtime/prebuilt/` and the
> cargo OUT_DIR `.so.zst` files before rebuilding when `python.rs` changes.
> The `build.sh` script does this automatically.

### Step 1: Build runtime.wasm (in container)

Compiles `eryx-wasm-runtime` to wasm32-wasip1 using nightly Rust with `-Z build-std`, links it with WASI SDK clang, and assembles the final WASM component from all base libraries.

```bash
container exec eryx-dev bash -c "
  cd /workspace &&
  WASI_SDK_PATH=/opt/wasi-sdk BUILD_ERYX_RUNTIME=1 \
    cargo build --package eryx-runtime --release
"
```

Output: `crates/eryx-runtime/runtime.wasm`

### Step 2: Build late-linking artifacts (in container)

Builds the WASM shared objects needed for preinit linking.

```bash
container exec eryx-dev bash -c "
  cd /workspace &&
  cargo build --package eryx-runtime --features preinit --release
"
```

### Step 3: Copy prebuilt artifacts (in container)

Copies the late-linking artifacts to the `prebuilt/` directory where the host precompile step can find them.

```bash
container exec eryx-dev bash -c "
  cd /workspace &&
  mkdir -p crates/eryx-runtime/prebuilt &&
  OUT_DIR=\$(find target/release -path '*/eryx-runtime-*/out/liberyx_runtime.so.zst' -type f | head -1 | xargs dirname) &&
  cp \$OUT_DIR/liberyx_runtime.so.zst crates/eryx-runtime/prebuilt/ &&
  cp \$OUT_DIR/liberyx_bindings.so.zst crates/eryx-runtime/prebuilt/
"
```

Output: `crates/eryx-runtime/prebuilt/{liberyx_runtime,liberyx_bindings}.so.zst`

### Step 4: Extract python-stdlib (in container or on host)

The precompile step needs the extracted Python standard library. If not already present:

```bash
container exec eryx-dev bash -c "
  cd /workspace/crates/eryx-wasm-runtime &&
  mkdir -p tests/python-stdlib &&
  rm -rf tests/python-stdlib-tmp &&
  mkdir -p tests/python-stdlib-tmp &&
  tar -xf ../eryx/python-stdlib.tar.zst -C tests/python-stdlib-tmp &&
  rm -rf tests/python-stdlib &&
  mv tests/python-stdlib-tmp/python-stdlib tests/python-stdlib &&
  rm -rf tests/python-stdlib-tmp
"
```

### Step 5: Precompile to native (on the host)

This step must run on the host because it produces platform-specific native code.

```bash
ERYX_PRECOMPILE_BOOTSTRAP=1 cargo run -p eryx-precompile --release -- compile \
  crates/eryx-runtime/runtime.wasm \
  --preinit \
  --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \
  --target native \
  --output crates/eryx-runtime/runtime.cwasm \
  --no-verify
```

Output: `crates/eryx-runtime/runtime.cwasm`

### Step 6: Run tests (on the host)

```bash
cargo nextest run --workspace --features embedded
```

## All-in-one script

`build.sh` runs steps 1-4 inside the container. After it completes, run steps 5-6 on the host.

```bash
container exec eryx-dev /workspace/container/build.sh
# Then on host:
ERYX_PRECOMPILE_BOOTSTRAP=1 cargo run -p eryx-precompile --release -- compile \
  crates/eryx-runtime/runtime.wasm --preinit \
  --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \
  --target native --output crates/eryx-runtime/runtime.cwasm --no-verify
```

## Container management

```bash
container list                    # Check running containers
container stop eryx-dev           # Stop the container (preserves cargo cache)
container start eryx-dev          # Restart it later
container rm eryx-dev             # Delete it (loses cargo cache inside the container)
```

The cargo build cache lives inside the container's filesystem, so stopping and restarting preserves it for incremental builds. Deleting the container loses the cache.
