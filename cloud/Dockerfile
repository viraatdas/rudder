FROM node:22-slim AS builder

WORKDIR /app

COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund

COPY tsconfig.json ./
COPY src ./src
RUN npm run build && npm prune --omit=dev --no-audit --no-fund

FROM node:22-slim

WORKDIR /app

RUN mkdir -p /data \
  && chown node:node /data

COPY --from=builder --chown=node:node /app/package.json /app/package-lock.json ./
COPY --from=builder --chown=node:node /app/node_modules ./node_modules
COPY --from=builder --chown=node:node /app/dist ./dist

ENV NODE_ENV=production
ENV PORT=3000
ENV RUDDER_CLOUD_DATA_DIR=/data

EXPOSE 3000

USER node
CMD ["node", "dist/server.js"]
