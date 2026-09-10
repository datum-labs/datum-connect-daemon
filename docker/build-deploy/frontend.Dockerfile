# ---- Build stage ----
FROM node:18-alpine AS builder

WORKDIR /app

# REACT_APP_API_URL must be baked in at build time by react-scripts
ARG REACT_APP_API_URL=http://localhost:5000
ENV REACT_APP_API_URL=$REACT_APP_API_URL

# Build context is repo root; source lives in frontend/
COPY frontend/package*.json ./
# npm ci requires a lockfile; fall back to npm install if none exists
RUN npm install

COPY frontend/ .

RUN npm run build

# ---- Production stage ----
FROM nginx:1.25-alpine AS runner

# Copy built React app
COPY --from=builder /app/build /usr/share/nginx/html

# Copy custom Nginx config (context is repo root, so path is relative to that)
COPY build-deploy/nginx.conf /etc/nginx/conf.d/default.conf

EXPOSE 80

CMD ["nginx", "-g", "daemon off;"]
