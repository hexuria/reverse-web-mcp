# rwmcp: one command to build, run, report and verify.
#
#   make bench            arms D and E on every phase ≤3 task, 5 runs each
#   make bench ARMS=D,E,B2 RUNS=3 TASKS=T2,T3
#   make report RUN=results/<stamp>
#   make verify RUN=results/<stamp>
#   make app              run the target app on http://127.0.0.1:47310 and open the UI
#   make bench-ocx ARMS=D,E,B,B2 PLANNER=model   everything through opencodex + grok-4.6, no key

ARMS ?= D,E
RUNS ?= 5
# Model settings. Point BASE_URL at a local gateway (opencodex on :8080) to avoid any Anthropic key.
MODEL ?= claude-opus-5
EFFORT ?= medium
PLANNER ?= handwritten
BASE_URL ?=
PHASE ?= 3
LATENCY ?= 25
SURFACES ?= api
TASKS ?=
RUN ?= $(shell ls -td results/*/ 2>/dev/null | head -1)

.PHONY: build test gate bench report verify app sandbox clean

build:
	cargo build --release

test:
	cargo test --workspace

# fmt + clippy -D warnings + tests. Every commit must pass this.
gate:
	./scripts/gate.sh

bench: build
	./target/release/bench run --spawn --arms $(ARMS) --runs $(RUNS) --phase $(PHASE) --latency-ms $(LATENCY) --surfaces $(SURFACES) \
	  --planner $(PLANNER) --model $(MODEL) --effort $(EFFORT) $(if $(BASE_URL),--base-url $(BASE_URL),) $(if $(TASKS),--tasks $(TASKS),)

# The same, through opencodex on localhost:8080 with xAI Grok. No Anthropic key needed.
bench-ocx: build
	$(MAKE) bench BASE_URL=http://localhost:8080 MODEL=grok-4.6 ARMS=$(ARMS) RUNS=$(RUNS) PLANNER=$(PLANNER) TASKS=$(TASKS)

report:
	./target/release/bench report --run $(RUN)

verify:
	./target/release/bench verify --run $(RUN)

app: build
	./target/release/app --bind 127.0.0.1:47310 --seed 1

# The sandbox: app + headless browsers + virtual displays, nothing on the host screen.
sandbox:
	docker compose -f docker/compose.yml up --build

clean:
	rm -rf target results/*/
