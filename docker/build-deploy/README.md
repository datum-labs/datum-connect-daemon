# build-deploy

Self-contained Docker Compose setup that builds and runs all three services — **PostgreSQL**, **Node/Express backend**, and **React frontend (via Nginx)** — from a single command.

The original `docker-compose.yml` at the repo root is unchanged and remains available for local development with hot-reload.

---

## What's in here

| File | Purpose |
|---|---|
| `docker-compose.yml` | Orchestrates all three services |
| `backend.Dockerfile` | Two-stage build: compiles TypeScript, then runs only production deps |
| `frontend.Dockerfile` | Two-stage build: React production build served by Nginx |
| `nginx.conf` | Nginx config — serves the React SPA, proxies `/api/` to the backend |
| `.env.example` | Template for required environment variables |

---

## Quick start

```bash
# 1. Move into this folder
cd build-deploy

# 2. Create your env file
cp .env.example .env
# Edit .env and set secure values for POSTGRES_PASSWORD and JWT_SECRET

# 3. Build and start everything
docker compose up --build

# To run in the background
docker compose up --build -d
```

Services will be available at:

| Service | URL |
|---|---|
| Frontend | http://localhost:3000 |
| Backend API | http://localhost:5000 |
| PostgreSQL | localhost:5432 |

---

## How it works

- **db** starts first. A `healthcheck` ensures Postgres is accepting connections before the backend launches.
- **backend** compiles TypeScript in a builder stage, then only copies `dist/` and production `node_modules` into the final image — keeping it lean. Uploaded files are persisted in a named Docker volume (`uploads_data`).
- **frontend** builds the React app with `REACT_APP_API_URL` baked in at build time (React env vars must be present during `npm run build`). The resulting static files are served by Nginx on port 80, mapped to host port 3000. Nginx also proxies any request starting with `/api/` to the backend container.

---

## Useful commands

```bash
# View logs for all services
docker compose logs -f

# View logs for a single service
docker compose logs -f backend

# Stop everything
docker compose down

# Stop and remove volumes (wipes the database)
docker compose down -v

# Rebuild a single service after a code change
docker compose up --build backend
```

---

## Changing the API URL

`REACT_APP_API_URL` is a **build-time** variable. If you change it in `.env` you need to rebuild the frontend image:

```bash
docker compose up --build frontend
```
