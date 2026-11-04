# Common development tasks. `make help` lists them.
# The same recipes exist in the justfile for people who use `just`.

CARGO  ?= cargo
DOCKER ?= docker
IMAGE  ?= registry.gitlab.com/ihrz/wavyzon
TAG    ?= dev

.DEFAULT_GOAL := help
.PHONY: help fmt fmt-check lint test check build release run doc deny docker docker-run compose-up compose-down clean

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  \033[36m%-13s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

fmt: ## Format all crates
	$(CARGO) fmt --all

fmt-check: ## Check formatting without changing files
	$(CARGO) fmt --all --check

lint: ## Run clippy on everything, warnings are errors
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

test: ## Run the test suite
	$(CARGO) test --workspace --all-features

check: fmt-check lint test ## What CI runs: fmt-check, lint, test

build: ## Debug build of the wavyzon binary
	$(CARGO) build -p wavyzon

release: ## Release build (target/release/wavyzon)
	$(CARGO) build --release --locked -p wavyzon

run: ## Run the node with settings from .env
	$(CARGO) run -p wavyzon

doc: ## Build the API docs and open them
	$(CARGO) doc --workspace --no-deps --all-features --open

deny: ## Check licences, advisories, bans and sources (needs cargo-deny)
	$(CARGO) deny check

docker: ## Build the Docker image ($(IMAGE):$(TAG))
	$(DOCKER) build --build-arg VERSION=$(TAG) -t $(IMAGE):$(TAG) .

docker-run: ## Run the Docker image with .env on port 2333
	$(DOCKER) run --rm -it --env-file .env -p 127.0.0.1:2333:2333 $(IMAGE):$(TAG)

compose-up: ## Start with docker compose
	$(DOCKER) compose up -d --build

compose-down: ## Stop the docker compose stack
	$(DOCKER) compose down

clean: ## Remove build output
	$(CARGO) clean
	rm -rf public dist
