.DEFAULT_GOAL := help

CARGO := cargo
PNPM := pnpm

.PHONY: help
help: ## show this help
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9._-]+:.*?##/ {printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

.PHONY: schema.gen
schema.gen: ## regenerate Rust + TS schema from tools/schema.json
	$(CARGO) run --quiet -p schemagen -- --lang rust --out crates/knot-markdown/src/schema.rs
	$(CARGO) run --quiet -p schemagen -- --lang ts   --out web/src/features/editor/schema.ts

.PHONY: dev.preflight
dev.preflight: ## sanity-check .env against the config validator
	@if [ ! -f .env ]; then \
		echo "no .env found — copy .env.example to .env first"; \
		exit 2; \
	fi
	@set -a; . ./.env; set +a; \
		$(CARGO) run --quiet --bin knot-server -- migrate >/dev/null 2>&1 || { \
			echo "config rejected .env — run with KNOT_LOG_FORMAT=text and re-check fields"; \
			exit 2; \
		}; \
		echo ".env validated"

.PHONY: install
install: web/node_modules e2e/node_modules ## install frontend + e2e npm deps

# Incremental: only re-install when a lockfile or manifest changes.
web/node_modules: web/pnpm-lock.yaml web/package.json
	cd web && $(PNPM) install
	@touch $@

e2e/node_modules: e2e/pnpm-lock.yaml e2e/package.json
	cd e2e && $(PNPM) install
	@touch $@

.PHONY: dev
dev: compose.up web/node_modules ## boot Postgres + backend (cargo-watch) + frontend (Vite) — Ctrl+C tears down both
	@command -v cargo-watch >/dev/null 2>&1 || $(CARGO) install cargo-watch
	@echo ""
	@echo "  knot dev"
	@echo "  --------"
	@echo "  backend     http://localhost:3000"
	@echo "  frontend    http://localhost:5173"
	@echo "  metrics     http://localhost:9090/metrics"
	@echo ""
	@echo "  Ctrl+C to stop both."
	@echo ""
	cd web && $(PNPM) dev:all

.PHONY: dev.server
dev.server: ## run knot-server with cargo-watch (auto-restart on edit)
	@command -v cargo-watch >/dev/null 2>&1 || $(CARGO) install cargo-watch
	$(CARGO) watch -q -x "run --bin knot-server"

.PHONY: dev.web
dev.web: web/node_modules ## run the SPA via Vite on :5173 (proxies /api,/auth,/collab to :3000)
	cd web && $(PNPM) dev

.PHONY: spike.server spike.web
spike.server: ## (deprecated) alias for dev.server
	@echo "spike.server is deprecated; use 'make dev.server'"
	@$(MAKE) dev.server
spike.web: ## (deprecated) alias for dev.web
	@echo "spike.web is deprecated; use 'make dev.web'"
	@$(MAKE) dev.web

.PHONY: test
test: test.rust test.web ## run all unit/integration tests

.PHONY: test.rust
test.rust: ## run Rust tests
	$(CARGO) nextest run --workspace --all-features

.PHONY: test.web
test.web: web/node_modules ## run TS unit tests
	cd web && $(PNPM) test

.PHONY: e2e
e2e: e2e/node_modules ## run the playwright convergence test
	cd e2e && $(PNPM) playwright test

.PHONY: lint
lint: ## cargo clippy + cargo fmt --check + tsc --noEmit
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings
	$(CARGO) fmt --all -- --check
	cd web && $(PNPM) tsc --noEmit

.PHONY: fmt
fmt: ## cargo fmt + prettier write
	$(CARGO) fmt --all
	cd web && $(PNPM) prettier --write src

.PHONY: clean
clean: ## remove build artifacts
	$(CARGO) clean
	rm -rf web/node_modules web/dist e2e/node_modules

.PHONY: compose.up
compose.up: ## start dev compose (Postgres) in background
	docker compose -f deploy/compose/dev.yml up -d
	@echo "waiting for Postgres to be healthy..."
	@for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do \
		if docker compose -f deploy/compose/dev.yml ps postgres | grep -q "healthy"; then \
			echo "Postgres healthy"; exit 0; \
		fi; sleep 1; \
	done; \
	echo "Postgres did not become healthy in 15s"; exit 1

.PHONY: compose.down
compose.down: ## stop dev compose
	docker compose -f deploy/compose/dev.yml down

.PHONY: compose.logs
compose.logs: ## tail dev compose logs
	docker compose -f deploy/compose/dev.yml logs -f

.PHONY: compose.psql
compose.psql: ## psql into the dev Postgres
	docker compose -f deploy/compose/dev.yml exec postgres psql -U knot -d knot

.PHONY: db.reset
db.reset: compose.up ## truncate every app table — fast reset, keeps the container
	@docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "\
		TRUNCATE TABLE \
		  acl_invalidations, audit_events, doc_markdown_cache, \
		  doc_snapshots, doc_updates, document_grants, documents, \
		  sessions, workspace_members, users, workspaces, \
		  blobs, blob_bytes \
		CASCADE" >/dev/null
	@echo "db reset"

.PHONY: db.nuke
db.nuke: ## stop compose and delete the Postgres volume (full wipe — slower but truly fresh)
	docker compose -f deploy/compose/dev.yml down -v
	@echo "compose volumes removed; next 'make dev' starts from zero"

.PHONY: db.cleanup
db.cleanup: ## drop all leftover t_* test databases from the dev Postgres
	@docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d postgres -tAc \
		"SELECT datname FROM pg_database WHERE datname LIKE 't\\_%' ESCAPE '\\'" \
	| while read db; do \
		[ -z "$$db" ] && continue; \
		echo "drop $$db"; \
		docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d postgres -c "DROP DATABASE IF EXISTS \"$$db\"" >/dev/null; \
	done; \
	echo "done"

.PHONY: migrate.up
migrate.up: ## apply pending migrations (against $$DATABASE_URL or compose default)
	DATABASE_URL=$${DATABASE_URL:-postgres://knot:knot@localhost:5432/knot} \
		sqlx migrate run --source migrations

.PHONY: migrate.down
migrate.down: ## revert the most recent migration
	DATABASE_URL=$${DATABASE_URL:-postgres://knot:knot@localhost:5432/knot} \
		sqlx migrate revert --source migrations

.PHONY: migrate.info
migrate.info: ## show migration status
	DATABASE_URL=$${DATABASE_URL:-postgres://knot:knot@localhost:5432/knot} \
		sqlx migrate info --source migrations

.PHONY: migrate.create
migrate.create: ## scaffold migrations/<ts>_<NAME>.sql; usage: make migrate.create NAME=add_foo
	@if [ -z "$(NAME)" ]; then \
		echo "usage: make migrate.create NAME=<short_snake_name>" >&2; \
		exit 2; \
	fi
	@TS=$$(date -u +%Y%m%d%H%M%S); \
		FILE="migrations/$${TS}_$(NAME).sql"; \
		printf -- "-- %s\n-- Created %s\n\n" "$(NAME)" "$$(date -u +%Y-%m-%d)" > "$$FILE"; \
		echo "created $$FILE"

IMAGE_NAME ?= knot
IMAGE_TAG  ?= dev

.PHONY: image.build
image.build: ## build multi-arch image locally (requires docker buildx)
	docker buildx build \
	  --platform linux/amd64,linux/arm64 \
	  --tag $(IMAGE_NAME):$(IMAGE_TAG) \
	  --load \
	  .

.PHONY: image.build.host
image.build.host: ## build single-arch image for the host (faster, for smoke testing)
	docker build --tag $(IMAGE_NAME):$(IMAGE_TAG) .

.PHONY: image.smoke
image.smoke: image.build.host ## run a freshly-built image against dev-compose Postgres
	docker run --rm --network host \
	  -e KNOT_DATABASE_URL="postgres://knot:knot@localhost:5432/knot" \
	  -e KNOT_SESSION_KEY="test-key-32-bytes-aaaaaaaaaaaaaa" \
	  $(IMAGE_NAME):$(IMAGE_TAG)
