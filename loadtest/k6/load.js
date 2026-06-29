// Madar backend load test.
//
// Driven entirely by env vars (scripts/loadtest.sh sets them); run a single
// PROFILE per invocation:
//
//   k6 run -e BASE_URL=http://localhost:8085 -e TOKEN=<jwt> -e PROFILE=ramp load.js
//
// PROFILE ∈ { smoke, ramp, soak, spike, pos-day }. The runner loops them for
// `loadtest.sh all`. WRITE_RATIO is the fraction of iterations that POST an
// order (the heavy money-engine path); the rest are weighted reads.
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Counter } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://localhost:8085';
const TOKEN = __ENV.TOKEN || '';
const ORG_ID = __ENV.ORG_ID || ''; // only needed for a super-admin token
const PROFILE = __ENV.PROFILE || 'smoke';
const WRITE_RATIO = parseFloat(__ENV.WRITE_RATIO || '0.15');
const SLEEP = parseFloat(__ENV.SLEEP || '0'); // per-iter think time (s); 0 = push hard
const ORDERS = new Counter('orders_created');
const writeTrend = new Trend('order_create_ms', true);

// Fixed fixture UUIDs — must match scripts/seed_fuzz.sql + loadtest/seed_loadtest.sql.
const ORG = '00000000-0000-0000-0000-000000000001';
const BRANCH = '00000000-0000-0000-0000-000000000002';
const SHIFT = '00000000-0000-0000-0000-000000000020';
// Payment method name is matched case-sensitively against the seed ("Cash"/"Card").
const PAYMENT = 'Cash';
const ITEMS = [
  '00000000-0000-0000-0000-000000000006',
  '00000000-0000-0000-0000-000000000007',
];

// ── Per-PROFILE executor config ──────────────────────────────────────────────
// Numbers are deliberately modest: the target box is 1 vCPU / 4 GB shared with
// Postgres, so the interesting region (the knee) shows up at low concurrency.
const SCENARIOS = {
  smoke: {
    executor: 'constant-vus', vus: 2, duration: '20s',
  },
  ramp: {
    executor: 'ramping-vus', startVUs: 1, gracefulStop: '10s',
    stages: [
      { duration: '30s', target: 10 },
      { duration: '1m', target: 25 },
      { duration: '1m', target: 50 },
      { duration: '1m', target: 100 },
      { duration: '30s', target: 0 },
    ],
  },
  soak: {
    executor: 'constant-vus', vus: parseInt(__ENV.VUS || '10'),
    duration: __ENV.DURATION || '3m',
  },
  spike: {
    executor: 'ramping-vus', startVUs: 3, gracefulStop: '10s',
    stages: [
      { duration: '20s', target: 5 },
      { duration: '10s', target: 80 }, // sudden burst
      { duration: '40s', target: 80 },
      { duration: '10s', target: 5 },
      { duration: '20s', target: 0 },
    ],
  },
  'pos-day': {
    executor: 'constant-arrival-rate',
    rate: parseInt(__ENV.RATE || '30'), timeUnit: '1s',
    duration: __ENV.DURATION || '2m',
    preAllocatedVUs: 40, maxVUs: parseInt(__ENV.MAX_VUS || '200'),
  },
};

const scenario = SCENARIOS[PROFILE];
if (!scenario) {
  throw new Error(`unknown PROFILE '${PROFILE}' (smoke|ramp|soak|spike|pos-day)`);
}

export const options = {
  scenarios: { [PROFILE]: scenario },
  // Informative, not abort-on-fail: on a 1 vCPU box under the ramp/spike
  // profiles these WILL be exceeded — that's the signal, not a crash.
  thresholds: {
    http_req_failed: ['rate<0.02'],
    http_req_duration: ['p(95)<2000'],
  },
  summaryTrendStats: ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

// MULTI-TENANT: scripts/loadtest.sh --multitenant passes TENANTS as a JSON array
// of { org, branch, shift, item, token } (one per seeded org). Each iteration
// picks one at random, so reads/writes spread across tenants and different orgs'
// orders hit different shift locks → they parallelize. Unset → single tenant.
const TENANTS = __ENV.TENANTS ? JSON.parse(__ENV.TENANTS) : null;
const SINGLE = { org: ORG, branch: BRANCH, shift: SHIFT, item: ITEMS[0], token: TOKEN };

function pickTenant() {
  if (TENANTS && TENANTS.length) return TENANTS[Math.floor(Math.random() * TENANTS.length)];
  return SINGLE;
}

function authHeaders(token) {
  const h = { Authorization: `Bearer ${token}` };
  if (ORG_ID) h['X-Org-Id'] = ORG_ID; // only used by a super-admin single-tenant token
  return h;
}

function get(t, path, name) {
  const res = http.get(`${BASE}${path}`, { headers: authHeaders(t.token), tags: { name } });
  check(res, { [`${name} 2xx`]: (r) => r.status >= 200 && r.status < 300 });
  return res;
}

function createOrder(t) {
  const body = JSON.stringify({
    branch_id: t.branch,
    shift_id: t.shift,
    payment_method: PAYMENT,
    items: [{ menu_item_id: t.item, quantity: 1 + Math.floor(Math.random() * 3) }],
  });
  const res = http.post(`${BASE}/orders`, body, {
    headers: { ...authHeaders(t.token), 'Content-Type': 'application/json' },
    tags: { name: 'POST /orders' },
  });
  const ok = check(res, { 'POST /orders 2xx': (r) => r.status >= 200 && r.status < 300 });
  if (ok) { ORDERS.add(1); writeTrend.add(res.timings.duration); }
  return res;
}

export default function () {
  const t = pickTenant();
  if (Math.random() < WRITE_RATIO) {
    createOrder(t);
  } else {
    const r = Math.random();
    if (r < 0.45) get(t, `/menu-items?org_id=${t.org}`, 'GET /menu-items');
    else if (r < 0.65) get(t, `/categories?org_id=${t.org}`, 'GET /categories');
    else if (r < 0.80) get(t, `/branches?org_id=${t.org}`, 'GET /branches');
    else if (r < 0.95) get(t, '/orders?per_page=20', 'GET /orders');
    else get(t, '/health', 'GET /health');
  }
  if (SLEEP > 0) sleep(SLEEP);
}
