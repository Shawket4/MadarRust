# Demo playground — one-time VPS setup

Everything repeatable runs through CI/CD (the `deploy-demo` job builds the image
and `docker compose up`s the demo backend; the dashboard CI builds + ships the
demo bundle). These are the **one-time** prerequisites you run on the VPS over
SSH — after this, every push to `main` redeploys the demo automatically.

Demo is fully isolated from prod: separate container (port 8082), separate DB
(`madar_demo`), separate `.env`, separate vhosts. Prod is never touched.

## 1. Separate database (prod data is never shared)
```bash
# As the Postgres owner of the prod DB (reuses the same role, new database):
sudo -u postgres createdb -O madar madar_demo
# (or: psql -U madar -h localhost -c 'CREATE DATABASE madar_demo;')
```
Migrations run automatically at container startup; no manual schema steps.

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

## 4. Nightly fresh reset (cron)
Wipes the demo DB and restarts the container, which re-runs migrations on an empty
schema → pristine demo every morning. Safe because `madar_demo` is 100% disposable.
```bash
sudo crontab -e
# 4:00 AM Cairo daily:
0 2 * * * psql -U madar -h localhost -d madar_demo -c "DROP SCHEMA public CASCADE; CREATE SCHEMA public;" && docker compose -f /opt/madar-demo/docker-compose.demo.yml restart madar-demo-backend
```
(The in-app sweeper also GCs each visitor's expired org every few minutes during the day.)

## Verify
```bash
curl -sf http://127.0.0.1:8083/health && echo ok
curl -s "https://demo-api.madar-pos.cloud/demo/session?variant=full" -X POST | head -c 200
```
