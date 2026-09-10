# ---- Build stage ----
FROM node:18-alpine AS builder

WORKDIR /app

# Build context is repo root; source lives in backend/
COPY backend/package*.json ./
# npm ci requires a lockfile; fall back to npm install if none exists
RUN npm install

COPY backend/ .

RUN npm run build

# ---- Production stage ----
FROM node:18-alpine AS runner

WORKDIR /app

COPY backend/package*.json ./

# Production deps only
RUN npm ci --only=production && npm cache clean --force

# Copy compiled output from builder
COPY --from=builder /app/dist ./dist

# Ensure uploads directory exists
RUN mkdir -p uploads/documents

EXPOSE 5000

CMD ["node", "dist/server.js"]
