# NineSQL – developer shortcuts.
#
# Thin wrappers around the commands the project already uses (AGENTS.md, CI in
# .github/workflows/ci.yml, dev/README.md). Nothing here adds behaviour of its own.
#
#   make            list the targets (same as `make help`)
#   make check      everything CI runs that needs no database engine
#
# Works with GNU make (Linux, and the GNU make 3.81 that macOS ships as /usr/bin/make) and
# with BSD make: only portable syntax is used (no GNU functions, no conditionals).
# Run it from the repository root.
#
# Override any variable on the command line, e.g. `make test CARGO=cargo +1.89`,
# `make dev-up PROFILE=sql`, `make build TIMEOUT=1800`.

# ---------------------------------------------------------------------------------------------
# Variables
# ---------------------------------------------------------------------------------------------

CARGO       ?= cargo
PYTHON      ?= python3
COMPOSE     ?= docker compose
# Profile of dev/compose.yaml for dev-up: core, sql, nosql, heavy or all.
PROFILE     ?= core
# Time limit, in seconds, of long cargo commands (AGENTS.md "Long-running tasks"). The first
# GPUI build is long.
TIMEOUT     ?= 900
# Time limit, in seconds, given to dev/ready.py and to image pulls.
DEV_TIMEOUT ?= 900
# Engines whose integration tests must not be skipped (same list as the driver-integration
# CI job).
REQUIRE_ENGINES ?= postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb
# Wire-compatible engines of the sql profile that test-engines starts besides the core profile
# (same list as WIRE_ENGINES in the driver-integration CI job; about 2.3 GB of memory together).
WIRE_ENGINES ?= timescaledb,yugabytedb,questdb,tidb
# The same list separated by spaces, for `docker compose pull` (no GNU functions, see above).
WIRE_SERVICES ?= timescaledb yugabytedb questdb tidb
# Crates whose integration tests run against the dev containers (driver-integration CI job).
ENGINE_TEST_CRATES ?= -p ninesql-driver-pgwire -p ninesql-driver-mysqlwire \
                      -p ninesql-driver-sqlite -p ninesql-core -p ninesql-ssh

# Extra arguments appended to `cargo test` in test and test-engines-run (empty by default, as
# in CI), e.g. TEST_ARGS=--no-fail-fast to run every test binary even after a failure.
TEST_ARGS ?=
# Options of scripts/e2e-local.sh for `make e2e-local`, e.g. E2E_ARGS="--only engines".
E2E_ARGS ?=

COMPOSE_FILE_ARGS = -f dev/compose.yaml

# Runs a command under `timeout $(TIMEOUT)`, or `gtimeout` (macOS with GNU coreutils), or
# without a time limit when neither exists (plain macOS).
WITH_TIMEOUT = sh -c 'if command -v timeout >/dev/null 2>&1; then exec timeout "$$0" "$$@"; \
	elif command -v gtimeout >/dev/null 2>&1; then exec gtimeout "$$0" "$$@"; \
	else exec "$$@"; fi' $(TIMEOUT)

# The driver-integration test command. Not run through $(MAKE), so `make -n test-engines`
# only prints it.
ENGINE_TESTS = NINESQL_REQUIRE_ENGINES=$(REQUIRE_ENGINES) $(WITH_TIMEOUT) $(CARGO) test --locked $(ENGINE_TEST_CRATES) $(TEST_ARGS)

# Forbidden GPUI APIs under crates/ (ADR-0013), as checked by the deny CI job.
FORBIDDEN_GPUI_APIS = set_http_client|with_http_client|read_credentials|write_credentials|delete_credentials

.PHONY: help \
	build run release \
	fmt fmt-check clippy test deny md-links verify-patches verify-gpui-snapshot forbidden-apis check \
	dev-up dev-up-all dev-down dev-check dev-ps \
	test-engines test-engines-run e2e-local \
	deb deb-install-test deb-reproducible sbom flatpak-lint \
	clean deps-debian deps-macos

# ---------------------------------------------------------------------------------------------
##@ Help
# ---------------------------------------------------------------------------------------------

help: ## List the targets (default)
	@echo "Usage: make <target> [VARIABLE=value ...]"
	@awk 'BEGIN { FS = ":.*## " } \
		/^##@ / { printf "\n%s\n", substr($$0, 5); next } \
		/^[a-zA-Z0-9_-]+:.*## / { printf "  %-22s %s\n", $$1, $$2 }' Makefile
	@echo ""
	@echo "Variables (current value):"
	@echo "  CARGO=$(CARGO)  PYTHON=$(PYTHON)  COMPOSE=$(COMPOSE)"
	@echo "  PROFILE=$(PROFILE)  TIMEOUT=$(TIMEOUT)  DEV_TIMEOUT=$(DEV_TIMEOUT)"
	@echo "  REQUIRE_ENGINES=$(REQUIRE_ENGINES)"
	@echo "  WIRE_ENGINES=$(WIRE_ENGINES)"
	@echo "  TEST_ARGS=$(TEST_ARGS)  E2E_ARGS=$(E2E_ARGS)"

# ---------------------------------------------------------------------------------------------
##@ Build and run
# ---------------------------------------------------------------------------------------------

build: ## Build the whole workspace (debug)
	$(WITH_TIMEOUT) $(CARGO) build --workspace --locked

run: ## Build and start the ninesql application (needs a display)
	$(CARGO) run --locked -p ninesql-ui --bin ninesql

release: ## Build the ninesql binary in release mode
	$(WITH_TIMEOUT) $(CARGO) build --release --locked -p ninesql-ui --bin ninesql

# ---------------------------------------------------------------------------------------------
##@ Quality (what CI checks)
# ---------------------------------------------------------------------------------------------

fmt: ## Format every crate (rewrites files)
	$(CARGO) fmt --all

fmt-check: ## Check formatting without changing files
	$(CARGO) fmt --all --check

clippy: ## Lint the workspace, warnings as errors
	$(WITH_TIMEOUT) $(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings

test: ## Run the workspace tests (engine tests skip unreachable engines)
	$(WITH_TIMEOUT) $(CARGO) test --workspace --locked $(TEST_ARGS)

deny: ## cargo-deny: licenses, bans, sources, advisories (needs cargo-deny)
	$(WITH_TIMEOUT) $(CARGO) deny --locked check

md-links: ## Check the internal links of the Markdown files
	$(PYTHON) scripts/check-md-links.py

verify-patches: ## Check the patched third-party crates under crates/patches/
	scripts/verify-patches.sh

verify-gpui-snapshot: ## Check the GPUI crates against their upstream sources (network)
	scripts/verify-gpui-snapshot.sh

forbidden-apis: ## Refuse GPUI HTTP and credential APIs under crates/ (ADR-0013)
	@if git grep -nE '$(FORBIDDEN_GPUI_APIS)' -- crates/; then \
		echo "Forbidden GPUI API used under crates/ (ADR-0013)"; exit 1; \
	else echo "No forbidden GPUI API under crates/"; fi

check: fmt-check clippy test deny verify-patches forbidden-apis md-links dev-check ## Everything CI runs that needs no engine
	@echo "check: all passed"

# ---------------------------------------------------------------------------------------------
##@ Dev databases (dev/README.md; Docker with the Compose v2 plugin)
# ---------------------------------------------------------------------------------------------

dev-up: ## Start a profile and wait until it is seeded (PROFILE=core by default)
	$(PYTHON) dev/ready.py up $(PROFILE) --timeout $(DEV_TIMEOUT)

dev-up-all: ## Start every engine (about 8 GB of RAM for the heavy ones)
	$(PYTHON) dev/ready.py up all --timeout $(DEV_TIMEOUT)

dev-down: ## Stop every dev container and delete its data volumes
	$(PYTHON) dev/ready.py down

dev-check: ## Check engines.json against compose.yaml (starts nothing)
	$(PYTHON) dev/ready.py check

dev-ps: ## Show the state of the dev containers
	$(COMPOSE) $(COMPOSE_FILE_ARGS) --profile all ps --all

# ---------------------------------------------------------------------------------------------
##@ Integration tests (driver-integration CI job)
# ---------------------------------------------------------------------------------------------

test-engines: ## Start core and the wire engines, run driver/core/ssh tests, stop the engines
	@status=0; \
	$(WITH_TIMEOUT) $(COMPOSE) $(COMPOSE_FILE_ARGS) --profile core pull --quiet \
	&& $(WITH_TIMEOUT) $(COMPOSE) $(COMPOSE_FILE_ARGS) --profile sql pull --quiet $(WIRE_SERVICES) \
	&& $(PYTHON) dev/ready.py up core --timeout $(DEV_TIMEOUT) \
	&& $(PYTHON) dev/ready.py up sql --engines $(WIRE_ENGINES) --timeout $(DEV_TIMEOUT) \
	&& $(ENGINE_TESTS) \
	|| status=$$?; \
	$(PYTHON) dev/ready.py down; \
	exit $$status

test-engines-run: ## Run the same tests against engines that are already up
	$(ENGINE_TESTS)

# ---------------------------------------------------------------------------------------------
##@ Local end-to-end run (docs/tutorials/local-e2e.md; a Linux desktop with Docker)
# ---------------------------------------------------------------------------------------------

e2e-local: ## Checks, engine tests and a GUI smoke test, with a report to send back (E2E_ARGS=...)
	scripts/e2e-local.sh $(E2E_ARGS)

# ---------------------------------------------------------------------------------------------
##@ Packaging (packaging/README.md; Docker, cargo-deb and cargo-about)
# ---------------------------------------------------------------------------------------------

deb: ## Build the .deb in the Ubuntu 22.04 builder (output in target/debian/)
	TIMEOUT=$(TIMEOUT) packaging/deb/build.sh

deb-install-test: ## Install target/debian/*.deb on Debian 12 / 13 and Ubuntu 22.04 / 24.04
	packaging/deb/install-test.sh target/debian/ninesql_*_amd64.deb

deb-reproducible: ## Build the .deb twice (two paths) and compare the hashes
	TIMEOUT=$(TIMEOUT) packaging/deb/check-reproducible.sh

sbom: ## Write the CycloneDX SBOM of the binary to target/ninesql.cdx.json (needs cargo-cyclonedx)
	$(PYTHON) packaging/sbom.py target/ninesql.cdx.json

flatpak-lint: ## Check the Flatpak manifest and its permissions (no flatpak-builder needed)
	$(PYTHON) packaging/flatpak/lint-manifest.py

# ---------------------------------------------------------------------------------------------
##@ Helpers
# ---------------------------------------------------------------------------------------------

clean: ## Delete the cargo build directory (cargo clean)
	$(CARGO) clean

deps-debian: ## Print the system packages for Debian / Ubuntu (does not install)
	@echo "Rust 1.89 or newer through rustup (https://rustup.rs); the distribution rustc is too old."
	@echo "System packages (docs/10-supported-platforms.md), run as yourself:"
	@echo "  sudo apt install build-essential pkg-config clang cmake libvulkan-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libzstd-dev libasound2-dev"
	@echo "Dev databases: Docker Engine with the Compose v2 plugin, Python 3.9 or newer."
	@echo "make check, verify-patches and e2e-local: Python 3.11 or newer (tomllib), cargo-deny."

deps-macos: ## Print the macOS requirements for UI iteration (does not install)
	@echo "macOS is a development platform only (docs/10-supported-platforms.md)."
	@echo "Rust 1.89 or newer through rustup (https://rustup.rs)."
	@echo "Xcode Command Line Tools:  xcode-select --install"
	@echo "  The full Xcode app may be needed for the Metal shader step of the GPUI build."
	@echo "Dev databases: Docker Desktop or OrbStack, Python 3.9 or newer."
	@echo "Optional: GNU coreutils (brew install coreutils) gives gtimeout, used for TIMEOUT."
