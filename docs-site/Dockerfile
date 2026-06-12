# punktfunk documentation site — Fumadocs on TanStack Start, built with Bun, served by
# the Nitro `bun` preset bundle. Self-contained context:
#
#   docker build -t punktfunk-docs docs-site
#
# Runtime: PORT (default 3000).
FROM oven/bun:1 AS build
WORKDIR /app

COPY package.json bun.lock ./
RUN bun install --frozen-lockfile --ignore-scripts

COPY . .
# fumadocs-mdx's vite plugin emits the .source typegen + content collections during build.
RUN bun run build

FROM oven/bun:1-slim
WORKDIR /app
COPY --from=build /app/.output ./.output
USER bun
ENV PORT=3000
EXPOSE 3000
CMD ["bun", "run", ".output/server/index.mjs"]
