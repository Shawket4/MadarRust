# Load testing the Madar backend (VPS-shaped, local)

Drives the real release binary in Docker, resource-capped to mimic the
production VPS, and hits it with [k6](https://k6.io). One command:

```bash
scripts/loadtest.sh ramp        # build → boot → seed → run the "ramp" profile → teardown
```

## What it models

Production runs on a **1 vCPU / 4 GB** Hostinger box with **Postgres native on the
same host** and the backend in a container next to it. `docker-compose.loadtest.yml`
reproduces that contention:

- **Postgres + backend are both pinned to one core** (`cpuset: "0"`) — they
  time-share a single vCPU exactly as on the VPS.
- **Memory is capped** (Postgres 1.5 GB, backend 1 GB) to stay inside the 4 GB
  envelope with page-cache headroom.
- **k6 runs on the host (the Mac)**, outside the capped containers — it's the
  "internet", not part of the box under test.

### Fidelity caveat (read this before trusting absolute numbers)

An Apple-silicon core is **substantially faster** than a Hostinger vCPU (which is
itself often a throttled slice of a shared physical core). So:

- **Relative results are trustworthy** — which endpoint is the bottleneck, how
  throughput degrades as concurrency climbs, where the latency knee is.
- **Absolute latency/throughput is OPTIMISTIC** — treat it as an upper bound.

To pull the simulation toward the real box, throttle the shared core:

```bash
BACKEND_CPUS=0.5 DB_CPUS=0.4 scripts/loadtest.sh ramp
```

The honest way to calibrate is to run the same profile against the actual VPS
once and scale `BACKEND_CPUS` until the smoke-profile latency matches.

## Profiles

| Profile   | Executor                | Shape                                            |
|-----------|-------------------------|--------------------------------------------------|
| `smoke`   | 2 VUs, 20 s             | Sanity — does everything respond 2xx.            |
| `ramp`    | ramping VUs 1→100       | Find the capacity knee (latency/error blow-up).  |
| `soak`    | 10 VUs, 3 m             | Stability + memory behaviour under steady load.  |
| `spike`   | burst to 80 VUs         | Resilience + recovery from a sudden surge.       |
| `pos-day` | 30 req/s arrival rate   | Realistic mixed read/write restaurant traffic.   |
| `all`     | each of the above, in turn | Full sweep; one results file per profile.     |

```bash
scripts/loadtest.sh all          # everything
scripts/loadtest.sh soak --keep  # leave the stack up to poke at afterwards
scripts/loadtest.sh down         # tear it down
```

## Knobs

| Env           | Default | Effect                                                    |
|---------------|---------|-----------------------------------------------------------|
| `BACKEND_CPUS`| `1.0`   | CPU cap on the backend (lower → approximate weaker vCPU).  |
| `DB_CPUS`     | `1.0`   | CPU cap on Postgres.                                       |
| `WRITE_RATIO` | `0.15`  | Fraction of iterations that POST an order (money path).   |
| `RATE`        | `30`    | `pos-day` arrival rate (req/s).                           |
| `VUS`         | `10`    | `soak` concurrency.                                       |
| `DURATION`    | profile | Override `soak`/`pos-day` duration (e.g. `10m`).          |

## The workload

Authenticated as a seeded **org_admin** (token minted by `src/bin/fuzz-token`
with the load-test JWT secret). Each iteration is a weighted mix of reads
(`/menu-items`, `/categories`, `/branches`, `/orders`, `/health`) plus a
`WRITE_RATIO` chance of `POST /orders` — the full cost/discount/tax money engine.

The fixture (`scripts/seed_fuzz.sql` + `loadtest/seed_loadtest.sql`) seeds one org,
branch, two priced items, payment methods, and **one open shift**.

> **Write concurrency note:** order creation takes a per-`shift_id` advisory lock,
> so writes against the single seeded shift **serialize** — a faithful model of one
> till, but it caps write throughput. To load-test multiple concurrent tills, seed
> several open shifts (each needs a distinct teller user — one-open-shift-per-teller
> is DB-enforced) and have the VUs pick one.

## Interpreting the output

k6 prints (and tees to `loadtest/results/<profile>.txt`):

- `http_req_duration` p95/p99 — latency; watch where p95 crosses ~1–2 s in `ramp`.
- `http_req_failed` — error rate; a climb marks the capacity ceiling.
- `iterations` / `http_reqs` rate — throughput (req/s).
- `order_create_ms` / `orders_created` — the write path specifically.

The thresholds (`http_req_failed < 2%`, `p95 < 2 s`) are **informative** — under
`ramp`/`spike` on one vCPU they're *meant* to be breached; that breach is the result.
