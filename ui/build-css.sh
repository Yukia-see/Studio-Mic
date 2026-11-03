#!/usr/bin/env bash
# Regenerates ui/tailwind.css from the classes used in mic-studio-mockup.html.
# Run after adding Tailwind classes to the UI, then rebuild (build.rs inlines the result).
set -euo pipefail
cd "$(dirname "$0")"
npx --yes tailwindcss@3.4.17 -c tailwind.config.js -i tailwind.in.css -o tailwind.css --minify
