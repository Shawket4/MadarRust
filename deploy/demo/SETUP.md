# Demo playground — one-time VPS setup

Everything repeatable runs through CI/CD (the `deploy-demo` job builds the image
and `docker compose up`s the demo backend; the dashboard CI builds + ships the
demo bundle). These are the **one-time** prerequisites you run on the VPS over
SSH — after this, every push to `main` redeploys the demo automatically.

Demo is fully isolated from prod: separate container (port 8083; 8082 is taken
by pihole on this box), separate DB (`madar_demo`), separate `.env`, separate
vhosts. Prod is never touched.

## 1. Separate database (prod data is never shared)
```bash
# As the Postgres owner of the prod DB (reuses the same role, new database):
sudo -u postgres createdb -O madar madar_demo

# The container connects as `madar` to host.docker.internal/madar_demo from the
# docker bridge subnet — allow it in pg_hba.conf (mirrors the existing prod rule,
# scoped to the disposable demo DB), then reload:
HBA=$(sudo -u postgres psql -tAc "show hba_file")
echo "host    madar_demo    madar    172.16.0.0/12    scram-sha-256" | sudo tee -a "$HBA"
sudo -u postgres psql -c "select pg_reload_conf()"

# The fresh DB replays ALL migrations, incl. a pre-rebrand one that GRANTs to a
# legacy 'sufrix' role. Prod ran it when that role existed; recreate it cluster-
# wide (NOLOGIN = no login/security surface; survives the nightly wipe below):
sudo -u postgres psql -c "CREATE ROLE sufrix NOLOGIN"
```
Migrations then run automatically at container startup; no manual schema steps.

## 2. Demo backend env + compose dir
```bash
sudo mkdir -p /opt/madar-demo
sudo cp deploy/demo/.env.example /opt/madar-demo/.env
sudo nano /opt/madar-demo/.env     # set DB password (same as prod) + fresh JWT_SECRET
# docker-compose.demo.yml is copied here by the CI deploy-demo job.
```

## 3. nginx vhosts (the part you asked me to do via SSH — run these)
```bash
# This box uses sites-available + sites-enabled symlinks (not conf.d).
sudo cp deploy/demo/nginx-demo-api.conf /etc/nginx/sites-available/demo-api.madar-pos.cloud
sudo cp deploy/demo/nginx-demo.conf     /etc/nginx/sites-available/demo.madar-pos.cloud
sudo ln -sf /etc/nginx/sites-available/demo-api.madar-pos.cloud /etc/nginx/sites-enabled/
sudo ln -sf /etc/nginx/sites-available/demo.madar-pos.cloud     /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx          # additive — prod vhosts untouched
sudo certbot --nginx -d demo.madar-pos.cloud -d demo-api.madar-pos.cloud
```
DNS: point `demo` and `demo-api` A/AAAA records at the VPS first (certbot needs them).

## 4. Nightly fresh reset (systemd timer — this box has no cron)
Wipes the demo DB and restarts the container, which re-runs migrations on an empty
schema → pristine demo every morning. Safe because `madar_demo` is 100% disposable.
`runuser postgres` uses peer auth (no password); re-owning `public` to `madar` is
required on PG15+ after a schema drop.
```bash
sudo tee /etc/systemd/system/madar-demo-reset.service >/dev/null <<'EOF'
[Unit]
Description=Nightly reset of the Madar demo playground (wipe madar_demo + restart backend to re-seed)
After=postgresql.service docker.service
Wants=postgresql.service docker.service

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'runuser -u postgres -- psql -d madar_demo -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public; ALTER SCHEMA public OWNER TO madar; GRANT ALL ON SCHEMA public TO madar;" && /usr/bin/docker compose -f /opt/madar-demo/docker-compose.demo.yml restart madar-demo-backend'
EOF

sudo tee /etc/systemd/system/madar-demo-reset.timer >/dev/null <<'EOF'
[Unit]
Description=Run the Madar demo reset nightly (02:00 UTC)

[Timer]
OnCalendar=*-*-* 02:00:00
Persistent=true

[Install]
WantedBy=timers.target
EOF

sudo systemctl daemon-reload && sudo systemctl enable --now madar-demo-reset.timer
```
(The in-app sweeper also GCs each visitor's expired org every few minutes during the day.)

## Verify
```bash
curl -sf http://127.0.0.1:8083/health && echo ok
curl -s "https://demo-api.madar-pos.cloud/demo/session?variant=full" -X POST | head -c 200
```
