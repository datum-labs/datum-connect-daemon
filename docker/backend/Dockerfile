FROM node:18-alpine

WORKDIR /app

# Copy package files
COPY package*.json ./

# Install all dependencies (including devDependencies for build)
RUN npm install -g typescript
RUN npm install --save-dev @types/node @types/express @types/cors @types/helmet @types/bcryptjs @types/jsonwebtoken @types/multer @types/uuid @types/joi

# Copy source code
COPY . .

# Build the TypeScript code
RUN npm run build

# Remove dev dependencies to reduce image size
RUN npm ci --only=production && npm cache clean --force

# Create uploads directory
RUN mkdir -p uploads/documents

EXPOSE 5000

CMD ["npm", "start"]