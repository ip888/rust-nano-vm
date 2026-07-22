# Node 20 LTS + Playwright + Chromium headless. Target for agent tool
# calls that scrape / test / screenshot web pages.
#
# Uses the mcr.microsoft.com Playwright base which ships with
# Chromium + all system libraries pre-installed and pinned to the
# matching Playwright version. Avoids the "chromium browser not found"
# cold-start error when the browser download hasn't run yet.
FROM mcr.microsoft.com/playwright:v1.48.0-noble
WORKDIR /app
RUN npm install playwright@1.48.0 --no-audit --no-fund
CMD ["node"]
