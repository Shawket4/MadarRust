# Domains and routes on `madar-pos.cloud`

For whoever holds root on the production VPS (`187.124.33.153`) and edits nginx. It
describes what is there today, the single DNS record being added, what that record is
for, how TLS should be issued for it, and one rule that must never be broken. Where the
repository does not actually prove something, this document says so rather than guessing —
those are collected at the end under **Verify on the box**.

Everything on this domain resolves to one machine. There is no load balancer and no second
origin; nginx on that box decides what each hostname serves.

---

## 1. What is there now

Every one of these is an `A` record straight to `187.124.33.153`, except `www`, which is a
`CNAME` to the apex.

| Host | Serves | Port |
|---|---|---|
| `api` | The Rust backend (`madar-backend`), the whole API | `127.0.0.1:8081` |
| `demo-api` | A second copy of the same image with `DEMO_MODE=1` | `127.0.0.1:8083` |
| `demo` | Static dashboard bundle, demo build — `/var/www/madar-demo` | static |
| `get` | Static marketing/landing bundle — `/var/www/madar-get` | static + `/api` (unproven) |
| `legal` | Static legal documents — `/var/www/madar-legal` | static, no backend at all |
| `loyalty` | Static loyalty bundle — `/var/www/madar-loyalty`, plus `/api/` proxied | static + backend |
| `order` | Static ordering bundle — `/var/www/madar-order`, plus `/api` proxied | static + backend |
| `reservations` | Static bookings bundle — `/var/www/madar-reservations`, plus `/api` | static + backend |
| `sentry` | Error reporting — **not defined anywhere in these repositories** | undetermined |
| `@` (apex) | Static management dashboard — `/var/www/madar-dashboard` | static |
| `www` | Presumed the apex, by alias or redirect — **unproven** | undetermined |

Four of these have their nginx configuration committed and can be trusted as written:
`legal/legal.vhost`, `deploy/loyalty/nginx-loyalty.conf`, `deploy/demo/nginx-demo.conf` and
`deploy/demo/nginx-demo-api.conf`. The rest have no vhost in the repository at all. Their
rows above are assembled from the deploy workflow's `scp` targets, from the `VITE_API_URL`
each bundle is built with, and from the backend's own environment — good evidence for
*which application* answers a hostname, weak evidence for *how nginx is currently written*.
Read the live `/etc/nginx/sites-available/` before changing any of them.

Three points are worth more than a table row.

**The backend is one process.** There is no per-product service. `api`, and the `/api`
paths under `loyalty`, `order`, `reservations` and `get`, all reach the same binary, which
binds `0.0.0.0:8081` (`BIND_ADDR` in `.env.example` and in the production env snapshot) and
is published on the host as `127.0.0.1:8081` by `docker-compose.yml`. The demo stack is the
same image again, published on `127.0.0.1:8083`. Port `8082` is taken by an unrelated pihole
on this box and must not be used.

**The committed loyalty vhost disagrees with that.** `deploy/loyalty/nginx-loyalty.conf`
proxies `/api/` to `http://127.0.0.1:8080/` — port 8080, not 8081. Nothing in either
repository binds 8080, and the file has said 8080 since it was first committed. Either the
live vhost was hand-corrected on the box and the fix was never committed back, or
`loyalty.madar-pos.cloud/api/` has never worked, which cannot be true because passes issue.
Check the running configuration, and commit whatever it says back into the repository.

**Front-end origins are separated on purpose.** The loyalty, ordering and bookings bundles
each get their own hostname so that a page opened by anyone who scans a counter QR cannot
load admin code or share cookies, `localStorage` or a session token with the management
dashboard. That is a security boundary, not a deployment convenience. Do not consolidate
them behind one origin, and do not add a `location` to one that proxies another's admin
routes.

One more thing about the loyalty vhost, since it is easy to undo by accident: it
deliberately has **no** `location ~* \.pkpass$` block. A regex location beats a prefix one,
so such a block would capture `/api/public/loyalty/pass/…/apple.pkpass` and proxy it without
stripping `/api/`; the backend would 404 and the customer would get nothing. The backend
already sets the right content type and disposition. There is nothing to fix there.

---

## 2. The one record being added

```
A   *   187.124.33.153   TTL 300
```

That is the entire DNS change. Nothing existing is touched.

**Nothing existing changes, because explicit records win.** A wildcard is only consulted
when the resolver finds no node at all for the name being looked up. `api.madar-pos.cloud`
exists as a real node, so `*` is never reached for it; the same for every host in the table
above. The wildcard answers only names nobody has defined.

There is a corollary that bites people, so it is worth stating: this is *per name*, not per
record type. The moment any record of any type exists at a name, the wildcard stops
synthesising anything for that name. Put a lone `TXT` at `foo.madar-pos.cloud` for a domain
verification and `foo.madar-pos.cloud` immediately stops resolving to an address, because
the node now exists and has no `A`. If you add a `TXT` to a name that is relying on the
wildcard, add the `A` explicitly at the same time.

**The wildcard does not reach into an existing host.** `rue.madar-pos.cloud` resolves
through it. `rue.loyalty.madar-pos.cloud` does not — the nearest existing node above it is
`loyalty.madar-pos.cloud`, so the only wildcard that could answer would be
`*.loyalty.madar-pos.cloud`, and that does not exist. The lookup is `NXDOMAIN`.

Being exact about the general rule, because the practical limit and the DNS rule are not
the same limit: a wildcard *can* synthesise for names more than one label deep, as long as
no real node sits between. `a.b.madar-pos.cloud` would resolve through `*`, since
`b.madar-pos.cloud` does not exist. The reason nothing deeper is usable in practice is TLS,
not DNS — a `*.madar-pos.cloud` certificate covers exactly one label, so `a.b` would resolve
and then fail the handshake. Treat one label as the working limit; just do not be surprised
when a two-label name answers a `dig`.

TTL 300 is deliberate. This record is how a new shop's hostname starts working, and five
minutes is the difference between provisioning being instant and being a support ticket.

---

## 3. What the wildcard is for

A shop on the branding tier gets its own hostname, its slug as the label:
`rue.madar-pos.cloud`. That host serves the customer-facing bundles for that one shop, with
the **product in the path**:

```
rue.madar-pos.cloud/card/<token>     the member's own loyalty card
rue.madar-pos.cloud/join/...          the counter QR's signup form
rue.madar-pos.cloud/order             ordering
rue.madar-pos.cloud/book              table bookings
```

So the hostname says *whose*, and the path says *what*. This is the inverse of the current
arrangement, where the hostname says what (`order`, `reservations`, `loyalty`) and the shop
is a query parameter or a token. Both will exist side by side: the generic hosts do not go
away, and a shop without the branding tier keeps using them.

The backend resolves the shop from the `Host` header. nginx should therefore pass `Host`
through unmodified (`proxy_set_header Host $host;` — the committed vhosts already do) and
must not rewrite it to an upstream name. One server block with
`server_name ~^(?<slug>[a-z0-9-]+)\.madar-pos\.cloud$;` and a root serving the combined
customer bundle is the shape to aim for; it must sort *after* the explicit `server_name`
blocks, which nginx does on its own — an exact `server_name` always beats a regex one.

**Only organisations on the branding tier get a hostname.** That gate already exists in the
database as `organizations.custom_branding`, set by a super admin only. It is also why a
branded shop's slug is frozen: once the slug is in a hostname it is printed on counter
cards, window stickers and posters that cannot be recalled, so it stops being editable
(`src/orgs/slugs.rs`, `is_frozen`).

**One caveat, and it is a large one: the backend half of this does not exist yet.** Slug
validation and the reserved list are written. Nothing in the backend reads the `Host` header
or maps a slug to an organisation — there is no such code in `src/`. The DNS record and an
nginx block can be put in place now and will serve the bundles, but requests will not resolve
to a shop until that lookup is built. Do not switch a live shop's printed QR codes to a
per-shop hostname before it is.

---

## 4. TLS

DNS is one record. Certificates are the actual decision, and there are two coherent ways to
do it.

### Option A — on-demand, per hostname

Put Caddy in front on 80/443 with on-demand TLS, and proxy everything back to nginx on
loopback. Caddy issues a certificate the first time a hostname is requested, over HTTP-01,
after asking the backend whether that hostname belongs to a real shop.

That `ask` endpoint is not optional and is not a nicety. Without it, anyone who points a
hostname of their own at `187.124.33.153` makes this box try to issue a certificate for it,
which is both a denial of service and a way to burn the rate limit below into the ground. It
must answer from the shop table, reject anything unknown fast, and cache both answers.

- **No DNS API is needed at all.** HTTP-01 only requires that the name resolves here and
  that port 80 reaches the challenge path. The wildcard record already guarantees the first.
- **It is the only option that extends to a shop's own domain.** When a café eventually
  wants `rewards.ruecoffee.com` pointed at us, on-demand issues for it the same way it
  issues for `rue.madar-pos.cloud`. A `*.madar-pos.cloud` wildcard can never cover a name
  outside `madar-pos.cloud`.
- **The rate limit is the real cost.** Let's Encrypt allows **50 certificates per registered
  domain per week**, and every `<slug>.madar-pos.cloud` counts against `madar-pos.cloud`'s
  50. Renewals count too. Fifty new branded shops in one week is far beyond the plausible
  signup rate, but it is a ceiling that exists, it is invisible until it is hit, and a
  misconfigured `ask` endpoint reaches it in minutes. Monitor issuance, and alert well below
  50.
- The cost in moving parts is real: TLS termination moves out of nginx, nginx becomes an
  upstream, and the `/.well-known/acme-challenge/` carve-outs in the existing vhosts stop
  being the thing that renews them.

### Option B — one `*.madar-pos.cloud` wildcard certificate

Keep nginx as it is and give it a wildcard certificate from certbot. One certificate, one
renewal, no per-hostname issuance, no rate-limit exposure worth thinking about.

The catch is the validation method. **Let's Encrypt will not issue a wildcard over HTTP-01.
Wildcards require DNS-01**, which means something has to write a `_acme-challenge` TXT
record into the authoritative zone every renewal. The nameservers for this domain are
Hostinger's `lunar.dns-parking.com` and `solar.dns-parking.com`, and **it is not established
that certbot has a working plugin for Hostinger's DNS**. Nobody has verified this. If you
take this option, verify it *first* — before moving anything else — because everything below
depends on the answer.

If there is no usable plugin, the fallbacks, in the order they are worth trying:

1. **Delegate the challenge.** `CNAME _acme-challenge.madar-pos.cloud` to a zone hosted
   somewhere with a real API — a small acme-dns instance, or a throwaway zone at a provider
   certbot supports. This is the standard escape hatch, it is a one-time change, and it
   leaves the rest of the domain exactly where it is. This is the right answer if Option B
   is chosen and the plugin does not exist.
2. **A `--manual` hook script** against Hostinger's own API, if it has one that can write
   TXT records. Workable, but it is bespoke code on the renewal path, which is the worst
   place for bespoke code.
3. **Manual DNS-01**, a person editing a TXT record every sixty days. This will be
   forgotten, and the failure mode is every branded shop's certificate expiring at once.
   Not acceptable as a steady state.
4. **Move the zone** to a provider with a first-class certbot plugin. The largest change,
   and it drags mail and everything else along with it.

A wildcard also covers exactly one label, so nothing at `a.b.madar-pos.cloud` will ever
work under it, and it can never cover a custom domain.

### Recommendation

**Take Option A.** The deciding argument is not elegance, it is that Option A has no
unknowns in it. Option B's viability rests on a DNS plugin nobody has confirmed exists, and
the fallback for that is either a delegation change or bespoke code on the renewal path.
Option A needs only what is already true: the wildcard record resolves, and port 80 reaches
the box. It is also the only path that reaches per-shop custom domains, which is where this
is going anyway.

Two conditions on that recommendation. The `ask` endpoint is load-bearing security and must
be built and tested before the first certificate is issued on demand. And issuance must be
monitored against the 50-per-week ceiling from day one, not after the first outage.

If issuance volume ever does approach that ceiling, the endgame is both: a wildcard
certificate for `*.madar-pos.cloud` via the CNAME delegation in Option B, covering every
slug host at the cost of one issuance per quarter, with on-demand kept alive purely for
custom domains.

---

## 5. The rule that cannot be broken: pass URLs are permanent

**`loyalty.madar-pos.cloud` must keep answering, at that exact name, with a publicly
trusted certificate, for as long as a single issued wallet pass exists.** It cannot be
retired, renamed, folded into a per-shop host, or replaced by a redirect.

The reason is that a wallet pass is not a page. It is a file that was written once, signed,
and handed to a customer's phone, and it carries absolute URLs *inside* it:

- **Apple.** A `.pkpass` contains `webServiceURL`. Every device that has the pass calls that
  URL forever — to register, to ask whether the pass changed, and to fetch the new copy
  after a push. It is inside the signed bundle. There is no mechanism to change it, and the
  reason is circular in the worst way: the only channel for delivering a corrected pass to a
  phone *is* the web service the pass already points at. If that host stops answering, every
  pass issued under it is frozen at the balance it had, silently, with no error the customer
  will ever see. A redirect does not save you either — Apple's client is strict about the
  chain it is talking to, which is also why the certificate can never be self-signed.
- **Google.** The card lives on Google's servers, and the class and object hold **absolute
  image URLs** — the shop's logo and hero image — which Google's own infrastructure fetches
  on a schedule we do not control and cannot predict. If those URLs stop resolving, every
  card of that class goes blank in every customer's wallet at once, and there is no
  invalidation we can trigger to fix it.

In this codebase those URLs are built by `loyalty::wallet::absolute_api_url`, which prefers
`PUBLIC_LOYALTY_BASE_URL` and appends `/api`, and falls back to the origin of
`UPLOADS_BASE_URL`. So `webServiceURL` is `https://loyalty.madar-pos.cloud/api/wallet` and
the images are `https://loyalty.madar-pos.cloud/api/public/loyalty/brand/...` — *provided
`PUBLIC_LOYALTY_BASE_URL` is set in production*. If it is not, both fall back to
`https://api.madar-pos.cloud/…` and it is `api` that becomes the permanent host instead.
Confirm which before assuming; the redacted production environment snapshot in this
repository does not set it.

What this means for nginx, concretely:

- The `/api/` proxy on `loyalty.madar-pos.cloud` stays. It carries the pass web service
  (`/api/wallet/*`), the pass download, and the brand images Google fetches. Removing it
  breaks passes already in wallets, not just new ones.
- Whatever host is baked into passes today keeps a valid publicly-trusted certificate
  permanently. An expired certificate there is not a broken page; it is every iPhone
  quietly ceasing to update.
- A per-shop host may serve the card **page** — `rue.madar-pos.cloud/card/<token>` is fine,
  it is an ordinary web page rendered fresh on each visit. It must never become the pass's
  `webServiceURL` or the source of the images Google fetches. Those stay on the permanent
  host, for every shop, forever.
- If a per-shop bundle is ever built that issues passes, it must go on constructing these
  URLs from `PUBLIC_LOYALTY_BASE_URL` and not from the request's own `Host`. A pass issued
  from `rue.madar-pos.cloud` that points back at `rue.madar-pos.cloud` welds that shop's
  slug into a signed file on a customer's phone — and slugs, printed QR codes
  notwithstanding, are the one thing here that a support conversation can still change.

---

## 6. Reserved subdomains

A shop's slug becomes a hostname, so a slug that collides with something we run takes that
service down. The list below is refused at organisation creation — for every shop, not only
branded ones, because a shop can be put on the tier at any time and discovering then that
its name was never usable is a worse conversation.

**Live today** (a shop taking one of these takes down that service):

```
api  demo  demo-api  get  legal  loyalty  order  reservations  sentry  www
autoconfig  autodiscover
```

**Mail, and the names the internet expects to find:**

```
mail  smtp  imap  pop  mx  ns  ns1  ns2  ftp
postmaster  hostmaster  webmaster  abuse  noreply  no-reply
```

**Ours, and ours to keep:**

```
madar  madarpos  admin  app  dashboard  portal  staff  pos  kds
auth  login  sso  id  account  accounts  my
billing  pay  payments  invoice  status
docs  help  support  blog  security  official
root  internal  ops  metrics  grafana  prometheus  logs
webhook  webhooks  git  ci  vpn
```

**Products not built yet** — cheap to reserve now, expensive to reclaim from a shop that has
printed it on a thousand receipts:

```
orders  booking  bookings  book  menu  track  tracking
qr  link  links  go  s  shop  store
cdn  static  assets  img  images  files  uploads
```

**Environments:**

```
test  dev  stage  staging  prod  beta  preview
```

And three rules that reserve shapes rather than names:

- **One and two characters are reserved wholesale.** They are what a short-link product will
  want, and they are the first thing anyone squats.
- **Anything beginning `xn--` is refused.** That is punycode, and a homograph of a real
  shop's name served from our own domain is a phishing page we hosted.
- **All-numeric slugs are refused.** They read as identifiers and will collide with anything
  path-shaped added later.

The authoritative copy is the `RESERVED` constant in `src/orgs/slugs.rs`, deliberately a
constant in the code rather than a table: it is a property of *our* deployment, not of any
tenant's data, and it has to be correct on the first request after a cold boot. If you add a
hostname to this domain, add it to that list in the same change.

---

## 7. Verify on the box

Things this repository does not settle. None of them block the DNS record; all of them
should be answered before nginx is restructured.

1. **Which port the live `loyalty.madar-pos.cloud` vhost proxies to.** The committed
   template says `8080`; everything else says the backend is on `8081`. Commit the truth
   back.
2. **Whether `PUBLIC_LOYALTY_BASE_URL` is set in the production environment.** This decides
   whether `loyalty` or `api` is the host welded into every issued pass — that is, which
   host section 5 applies to.
3. **What `www` actually does.** Its only trace in either repository is a CORS origin
   string. Alias of the apex, redirect to it, or a leftover — unknown.
4. **What serves `sentry`.** Nothing in either repository defines it; it appears only as a
   DSN and a source-map upload target. It is administered outside these repositories.
5. **Whether the apex still proxies `/api`.** The dashboard's environment files say it no
   longer does, while `.env.example` sets `UPLOADS_BASE_URL=https://madar-pos.cloud/api/uploads`,
   which requires that it does. The production snapshot points uploads at `api` instead. At
   least one of these is stale.
6. **The vhosts with no committed configuration** — `api`, `get`, `order`, `reservations`,
   the apex and `www`. Copy what is running into `deploy/` so the next person is not
   reconstructing it from `scp` targets.
7. **Whether certbot has a Hostinger DNS plugin**, if and only if Option B is under serious
   consideration. Everything in Option B depends on that answer.
