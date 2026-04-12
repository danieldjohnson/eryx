#!/bin/bash
# Build WASM artifacts inside the container.
# Run this inside the container: /workspace/container/build.sh
#
# This builds the platform-independent WASM artifacts (steps 1-4).
# After this completes, run the precompile on the HOST to produce
# a native runtime.cwasm:
#
#   ERYX_PRECOMPILE_BOOTSTRAP=1 cargo run -p eryx-precompile --release -- compile \
#     crates/eryx-runtime/runtime.wasm --preinit \
#     --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \
#     --target native --output crates/eryx-runtime/runtime.cwasm --no-verify
set -euo pipefail

cd /workspace

echo "=== Step 0: Clear stale late-linking artifacts ==="
# Both runtime.wasm and liberyx_runtime.so contain compiled python.rs code.
# If we don't clear these, cargo may reuse stale cached .so files that
# don't reflect the latest source changes.
rm -f crates/eryx-runtime/prebuilt/*.so.zst
find target/release -path "*/eryx-runtime-*/out/*.so.zst" -delete 2>/dev/null || true
find target/release -path "*/eryx-runtime-*/out/*.so" -delete 2>/dev/null || true

echo ""
echo "=== Step 1: Build runtime.wasm ==="
WASI_SDK_PATH=/opt/wasi-sdk BUILD_ERYX_RUNTIME=1 \
  cargo build --package eryx-runtime --release

echo ""
echo "=== Step 2: Build late-linking artifacts ==="
cargo build --package eryx-runtime --features preinit --release

echo ""
echo "=== Step 3: Copy prebuilt artifacts ==="
mkdir -p crates/eryx-runtime/prebuilt
OUT_DIR=$(find target/release -path "*/eryx-runtime-*/out/liberyx_runtime.so.zst" -type f | head -1 | xargs dirname)
cp "$OUT_DIR/liberyx_runtime.so.zst" crates/eryx-runtime/prebuilt/
cp "$OUT_DIR/liberyx_bindings.so.zst" crates/eryx-runtime/prebuilt/
echo "Copied to crates/eryx-runtime/prebuilt/"
ls -la crates/eryx-runtime/prebuilt/

echo ""
echo "=== Step 4: Extract python-stdlib ==="
cd crates/eryx-wasm-runtime
mkdir -p tests/python-stdlib tests/site-packages

PYTHON_LIB="../eryx/python-stdlib.tar.zst"
rm -rf tests/python-stdlib-tmp
mkdir -p tests/python-stdlib-tmp
tar -xf "$PYTHON_LIB" -C tests/python-stdlib-tmp
rm -rf tests/python-stdlib
mv tests/python-stdlib-tmp/python-stdlib tests/python-stdlib
rm -rf tests/python-stdlib-tmp
cd /workspace

echo ""
echo "=== Done! ==="
echo "WASM artifacts built. Now run on the HOST:"
echo ""
echo "  ERYX_PRECOMPILE_BOOTSTRAP=1 cargo run -p eryx-precompile --release -- compile \\"
echo "    crates/eryx-runtime/runtime.wasm --preinit \\"
echo "    --stdlib crates/eryx-wasm-runtime/tests/python-stdlib \\"
echo "    --target native --output crates/eryx-runtime/runtime.cwasm --no-verify"
