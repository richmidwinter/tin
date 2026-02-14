# Thumbnail Service

Local thumbnail generation service for browser extensions. Renders website screenshots using headless Chrome/Chromium/Brave.

## Requirements

- Rust 1.70+
- Chrome, Chromium, or Brave browser

## Build

    cargo build --release

## Run

    ./target/release/thumbnail-service

Service binds to `127.0.0.1:9142` by default. Set `PORT` env var to change.

## Test

Health check:

    curl http://localhost:9142/health

Generate thumbnail and display in iTerm2 with imgcat:

    curl -s "http://localhost:9142/thumbnail?url=https://news.ycombinator.com&width=640&height=400&format=png" | jq -r '.image_data' | base64 -d | imgcat

## API

### GET /thumbnail

Query parameters:
- `url` (required): Target URL
- `width` (default: 640): Output width
- `height` (default: 400): Output height
- `format` (default: webp): `webp`, `jpeg`, or `png`

Returns JSON with base64-encoded image.

### POST /thumbnail

Same parameters as JSON body.

### GET /health

Returns service status and browser availability.

## Browser Detection

Searches for browsers in this order:
1. Google Chrome
2. Brave Browser
3. Chromium (Homebrew and system)
4. Chrome Canary

Install Chromium via Homebrew if you don't want to use your main browser:

    brew install chromium

