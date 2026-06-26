# punktfunk management console — TanStack Start built with Bun, served by the Nitro `bun`
# preset bundle. Build context is the REPO ROOT (orval generates the API client from
# api/openapi.json, referenced as ../api/openapi.json from web/):
#
#   docker build -f web/Dockerfile -t punktfunk-web .
#
# Runtime: PORT (default 3000) and PUNKTFUNK_MGMT_URL (upstream management API the Nitro
# server proxies /api to; see web/server/routes).
FROM oven/bun:1 AS build
WORKDIR /repo/web

# Dependency layer: lockfile only, so source edits don't re-install.
# --ignore-scripts: the root `prepare` script runs codegen, which needs sources that
# aren't copied yet — `bun run build` regenerates everything below.
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile --ignore-scripts

COPY api/openapi.json /repo/api/openapi.json
COPY web/ ./
# prebuild runs orval (openapi → src/api/gen); the paraglide vite plugin compiles i18n.
RUN bun run build

FROM oven/bun:1-slim
WORKDIR /app
COPY --from=build /repo/web/.output ./.output
USER bun
ENV PORT=3000
EXPOSE 3000
CMD ["bun", "run", ".output/server/index.mjs"]
