default:
    @just --list

install-hooks:
    git config core.hooksPath .githooks

build *ARGS:
    cargo build {{ARGS}}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
check *ARGS:
    cargo check --workspace --tests --benches {{ARGS}}

run *ARGS:
    cargo run {{ARGS}}

install *ARGS:
    cargo install --locked --path . {{ARGS}}

test *ARGS:
    cargo nextest run --workspace {{ARGS}}

lint:
    cargo clippy --all --tests --benches -- -D warnings

lint-fix:
    cargo clippy --all --tests --benches --fix

fmt-check:
    cargo fmt --all -- --check
    stylua --check plugins/

fmt:
    cargo fmt --all
    stylua plugins/

pylint:
    ruff check scripts/
    ty check scripts/

gen-docs:
    cargo run -p maki-docgen

gen-docs-check:
    cargo run -p maki-docgen -- --check

machete:
    cargo-machete

# The Claude Agent bridge (TypeScript, Node 24+); not part of `ci`.
bridge-check:
    cd claude-agent-bridge && npm install --omit=optional && npm run check && npm test

# Criterion benches (maki-lua: luau_perf, splash_perf, provider_perf). Pass a filter to pick
# one, e.g. `just bench -- pull_roundtrip`; slow meters are sample_size 10.
bench *ARGS:
    cargo bench -p maki-lua {{ARGS}}

# Build the phone app's APK (arm64 + x86_64). Needs ANDROID_HOME with NDK and
# build-tools; a JDK for apksigner. Output: target/android/makima.apk
android-apk *ARGS:
    python maki-mobile/android/build_apk.py {{ARGS}}

# Install the debug APK on the attached device or emulator and start it.
android-run *ARGS:
    python maki-mobile/android/build_apk.py --debug --install {{ARGS}}

# Full CI check
ci: fmt-check lint pylint test gen-docs-check machete
