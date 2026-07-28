# transit — France multimodal GTFS / GraphQL / SIRI Lite / WASM map
#
# Usage:
#   make help          list targets
#   make run           API server (release, loads .env)
#   make web           WASM map UI (http://127.0.0.1:8088)
#   make dev           reminder: run API + web in two terminals
#
# Paths are relative to this Makefile (repo root = transit/).

.DEFAULT_GOAL := help

ROOT     := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
CARGO    ?= cargo
RUST_LOG ?= info,transit=info
BASE_URL ?= http://127.0.0.1:8080
WEB_DIR  := $(ROOT)/web
SCRIPTS  := $(ROOT)/scripts
BIN_REL  := $(ROOT)/target/release/transit
BIN_DBG  := $(ROOT)/target/debug/transit

export RUST_LOG
export PATH := $(HOME)/.cargo/bin:$(PATH)

.PHONY: help
help: ## Show this help
	@echo "transit — make targets"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Env: RUST_LOG=$(RUST_LOG)  BASE_URL=$(BASE_URL)"
	@echo "     API keys via .env — IDFM_PRIM_API_KEY, DATASETS_API_KEY (see .env.example)"

# ─── Setup ───────────────────────────────────────────────────────────────────

.PHONY: setup
setup: ## Install deps: wasm target, trunk (if missing), chmod scripts
	@rustup target add wasm32-unknown-unknown
	@if ! command -v trunk >/dev/null 2>&1; then \
		echo "trunk missing — will be installed by web/serve.sh on first make web"; \
	fi
	@chmod +x $(SCRIPTS)/*.sh $(WEB_DIR)/serve.sh 2>/dev/null || true
	@if [ ! -f $(ROOT)/.env ] && [ -f $(ROOT)/.env.example ]; then \
		cp -n $(ROOT)/.env.example $(ROOT)/.env && chmod 600 $(ROOT)/.env && \
		echo "created .env from .env.example (edit secrets)"; \
	fi
	@echo "setup ok"

.PHONY: env
env: ## Create .env from .env.example if missing
	@if [ -f $(ROOT)/.env ]; then echo ".env already exists"; \
	elif [ -f $(ROOT)/.env.example ]; then \
		cp $(ROOT)/.env.example $(ROOT)/.env && chmod 600 $(ROOT)/.env && echo "created .env"; \
	else \
		printf '%s\n' \
			'# PRIM SIRI Lite apikey (IDFM marketplace)' \
			'IDFM_PRIM_API_KEY=' \
			'# data.gouv.fr GTFS downloads (X-API-KEY); set one alias' \
			'DATASETS_API_KEY=' \
			'DATAGOUV_API_KEY=' \
			'RUST_LOG=info,transit=info' \
			> $(ROOT)/.env && chmod 600 $(ROOT)/.env && echo "wrote minimal .env"; \
	fi

# ─── Build ───────────────────────────────────────────────────────────────────

.PHONY: build
build: ## Debug build of transit binary + lib
	cd $(ROOT) && $(CARGO) build --bin transit

.PHONY: release
release: ## Release build (recommended for IDFM memory/CPU)
	cd $(ROOT) && $(CARGO) build --release --bin transit

.PHONY: check
check: ## cargo check (lib + bin, fast)
	cd $(ROOT) && $(CARGO) check --all-targets

.PHONY: build-web
build-web: ## Trunk release build of WASM UI → web/dist
	cd $(WEB_DIR) && rm -rf dist/.stage && \
		unset NO_COLOR CLICOLOR CLICOLOR_FORCE FORCE_COLOR 2>/dev/null; \
		TRUNK_COLOR=never TRUNK_SKIP_VERSION_CHECK=true trunk build --release

.PHONY: check-web
check-web: ## cargo check WASM client
	cd $(WEB_DIR) && $(CARGO) check --target wasm32-unknown-unknown

.PHONY: all
all: release build-web ## Release server + WASM UI

# ─── BAN addresses (Base Adresse Nationale) ──────────────────────────────────

.PHONY: ban-index
ban-index: ## Download BAN IDF CSV + build data/ban/index.bin (address autocomplete)
	cd $(ROOT) && $(CARGO) run -p ban-search --bin ban-index -- \
		--data-dir $(ROOT)/data/ban --download --build
	@echo "BAN index ready — restart API (make run) to load addresses"

.PHONY: ban-index-paris
ban-index-paris: ## Faster: BAN 75 only
	cd $(ROOT) && $(CARGO) run -p ban-search --bin ban-index -- \
		--data-dir $(ROOT)/data/ban --dept 75 --download --build

.PHONY: ban-suggest
ban-suggest: ## Test BAN suggest (QUERY="12 rue de rivoli")
	cd $(ROOT) && $(CARGO) run -p ban-search --bin ban-index -- \
		--data-dir $(ROOT)/data/ban --suggest "$(or $(QUERY),12 rue de rivoli paris)"

# ─── Run ─────────────────────────────────────────────────────────────────────

.PHONY: run
run: ## GraphQL API (release via scripts/run_server.sh, loads .env)
	cd $(ROOT) && $(SCRIPTS)/run_server.sh

.PHONY: run-debug
run-debug: ## GraphQL API debug build (loads .env if present)
	@if [ -f $(ROOT)/.env ]; then set -a; . $(ROOT)/.env; set +a; fi; \
	cd $(ROOT) && $(CARGO) run --bin transit

.PHONY: run-bin
run-bin: release ## Run prebuilt target/release/transit (SKIP_CARGO=1)
	cd $(ROOT) && SKIP_CARGO=1 $(SCRIPTS)/run_server.sh

.PHONY: web
web: ## WASM map UI (Trunk serve → http://127.0.0.1:8088)
	cd $(WEB_DIR) && ./serve.sh

.PHONY: serve
serve: web ## Alias for make web

.PHONY: dev
dev: ## Print how to run API + web together
	@echo "Terminal 1:  make run          # API  → $(BASE_URL)"
	@echo "Terminal 2:  make web          # map  → http://127.0.0.1:8088"
	@echo "Optional:    make sync-idfm    # download IDFM GTFS first"
	@echo "Addresses:   make ban-index    # BAN IDF → data/ban/index.bin"
	@echo "Smoke:       make smoke        # with API up"

# ─── Test / quality ──────────────────────────────────────────────────────────

.PHONY: test
test: ## All library + integration tests
	cd $(ROOT) && $(CARGO) test

.PHONY: test-lib
test-lib: ## Library unit tests only (fast)
	cd $(ROOT) && $(CARGO) test --lib

.PHONY: test-rt
test-rt: ## Realtime / estimate / SIRI / PRIM tests
	cd $(ROOT) && $(CARGO) test --lib estimate
	cd $(ROOT) && $(CARGO) test --lib prim::
	cd $(ROOT) && $(CARGO) test --lib adapter

.PHONY: test-quiet
test-quiet: ## Quiet lib tests
	cd $(ROOT) && $(CARGO) test -q --lib

.PHONY: test-e2e
test-e2e: ## Integration e2e (alerts, itinerary station nodes, labels)
	cd $(ROOT) && $(CARGO) test --test e2e_quality --test plan_tiny_gtfs

.PHONY: fmt
fmt: ## cargo fmt
	cd $(ROOT) && $(CARGO) fmt
	@if [ -f $(WEB_DIR)/Cargo.toml ]; then cd $(WEB_DIR) && $(CARGO) fmt; fi

.PHONY: fmt-check
fmt-check: ## cargo fmt --check
	cd $(ROOT) && $(CARGO) fmt -- --check

.PHONY: clippy
clippy: ## cargo clippy (all targets)
	cd $(ROOT) && $(CARGO) clippy --all-targets -- -D warnings

.PHONY: gate
gate: fmt-check test-lib check-web ## CI-style gate: fmt + lib tests + wasm check
	@echo "gate ok"

# ─── Data / IDFM ─────────────────────────────────────────────────────────────

.PHONY: sync-idfm
sync-idfm: ## Download/validate IDFM GTFS → data/idfm/current.zip
	cd $(ROOT) && $(SCRIPTS)/daily_sync.sh

.PHONY: sync-idfm-restart
sync-idfm-restart: ## daily_sync then hint to restart server (RESTART=1 if systemd)
	cd $(ROOT) && RESTART=1 $(SCRIPTS)/daily_sync.sh

.PHONY: repack-idfm
repack-idfm: ## Re-parse data/idfm/current.zip (trip aliases + shape_dist); restart server
	@echo "Restart transit to reload packed IDFM epoch from data/idfm/current.zip"
	@if systemctl is-active --quiet transit 2>/dev/null; then \
		sudo systemctl restart transit && echo "transit restarted"; \
	else \
		echo "No systemd transit unit — stop and re-run: make run"; \
	fi

.PHONY: idfm-lines
idfm-lines: ## Build IDFM line GeoJSON for the map (scripts/build_idfm_lines.py)
	cd $(ROOT) && python3 $(SCRIPTS)/build_idfm_lines.py

.PHONY: list-feeds
list-feeds: ## List PAN / transport.data.gouv feeds (GTFS / RT)
	cd $(ROOT) && $(SCRIPTS)/list_pan_feeds.sh

.PHONY: install-cron
install-cron: ## Install user crontab for daily IDFM sync (03:30)
	cd $(ROOT) && $(SCRIPTS)/install_cron.sh

.PHONY: install-cron-print
install-cron-print: ## Print crontab entry without installing
	cd $(ROOT) && $(SCRIPTS)/install_cron.sh --print

# ─── Smoke / ops ─────────────────────────────────────────────────────────────

.PHONY: smoke
smoke: ## Live smoke against running server (BASE_URL=…)
	cd $(ROOT) && BASE_URL=$(BASE_URL) $(SCRIPTS)/smoke_live.sh

.PHONY: health
health: ## curl GET /health
	@curl -fsS "$(BASE_URL)/health" | (command -v jq >/dev/null && jq . || cat)
	@echo

.PHONY: graphql-ping
graphql-ping: ## Minimal GraphQL __typename probe
	@curl -fsS -X POST "$(BASE_URL)/graphql" \
		-H 'content-type: application/json' \
		-d '{"query":"{ __typename }"}' | (command -v jq >/dev/null && jq . || cat)
	@echo

.PHONY: vehicles-sample
vehicles-sample: ## Sample live vehicles (idfm, limit 5)
	@curl -fsS -X POST "$(BASE_URL)/graphql" \
		-H 'content-type: application/json' \
		-d '{"query":"{ vehicles(feedId: \"idfm\", limit: 5) { tripId lat lon displayLabel mode currentStatus bearing } }"}' \
		| (command -v jq >/dev/null && jq . || cat)
	@echo

# ─── Docker ──────────────────────────────────────────────────────────────────

.PHONY: docker-build
docker-build: ## docker compose build
	cd $(ROOT) && docker compose build

.PHONY: docker-up
docker-up: ## docker compose up -d
	cd $(ROOT) && docker compose up -d

.PHONY: docker-down
docker-down: ## docker compose down
	cd $(ROOT) && docker compose down

.PHONY: docker-logs
docker-logs: ## docker compose logs -f
	cd $(ROOT) && docker compose logs -f

# ─── Clean ───────────────────────────────────────────────────────────────────

.PHONY: clean
clean: ## cargo clean (server)
	cd $(ROOT) && $(CARGO) clean

.PHONY: clean-web
clean-web: ## Remove web/dist and web/target
	rm -rf $(WEB_DIR)/dist $(WEB_DIR)/target

.PHONY: clean-all
clean-all: clean clean-web ## Clean server + web artifacts
	@echo "cleaned"

.PHONY: clean-data
clean-data: ## DANGER: remove downloaded GTFS under data/ (asks confirm)
	@echo "This deletes $(ROOT)/data/* feed caches (sncf/idfm zips)."
	@read -p "Type yes to continue: " a; [ "$$a" = "yes" ]
	rm -rf $(ROOT)/data/sncf $(ROOT)/data/idfm
	@echo "data feeds removed (re-run make sync-idfm / make run to redownload)"
