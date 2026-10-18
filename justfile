# Developer and CI tasks for sdigest. CI runs these same recipes, so a
# recipe that passes locally passes in CI. Run `just` to list them.

# List recipes
default:
    @{{ just_executable() }} --list

# Create .venv with the Python extension built, and install the git hooks
[group('setup')]
devready:
    uv sync --locked
    uv run prek install -f

# Check lockfiles, lint, formatting (all files; narrow with e.g. --from-ref origin/main)
[group('quality')]
cqa *args="--all-files":
    cargo metadata --locked --format-version 1 > /dev/null
    uv lock --locked
    uv run --only-group lint prek run {{ args }}

# Run all tests
[group('test')]
test: test-rust test-python

# Rust unit and CLI tests, with and without the CLI feature
[group('test')]
test-rust:
    cargo test --locked
    cargo test --locked --no-default-features

# Python tests (builds the extension first)
[group('test')]
test-python:
    uv run --locked pytest

# Check packaging: the crate package, plus an sdist and wheel for this platform in dist/
[group('build')]
build:
    rm -rf dist
    cargo package --locked --allow-dirty
    uv build

# Remove build outputs and caches (keeps .venv)
[group('cleanup')]
clean:
    cargo clean
    rm -rf dist .pytest_cache .ruff_cache
    find . -path ./.venv -prune -o -name __pycache__ -type d -exec rm -rf {} +

# Benchmark harness: e.g. `just bench report`, `just bench run --ref 0.2.1 ...`
[group('bench')]
bench *args:
    uv run --script bench/sdbench.py {{ args }}
