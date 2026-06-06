set shell := ["bash", "-cu"]

assets_dir := "assets"
tailwind := "./node_modules/.bin/tailwindcss"
css_in := "css/style.css"
css_out := "../src/web/assets/app.css"

# Default: list available recipes
default:
    @just --list

# Install JS deps (run once after cloning)
deps:
    cd {{assets_dir}} && npm install

# One-shot CSS build + sync vendored fonts. The output is committed:
# rust-embed bakes src/web/assets/ into the binary at cargo build time.
assets: fonts
    cd {{assets_dir}} && {{tailwind}} -c tailwind.config.js -i {{css_in}} -o {{css_out}}

# Watch CSS during development (fonts only need a sync per dep change)
watch: fonts
    cd {{assets_dir}} && {{tailwind}} -c tailwind.config.js -i {{css_in}} -o {{css_out}} --watch

# Copy vendored B612 Mono into the embedded assets tree. Fontsource's CSS
# uses url("./files/..."), which resolves to /files/* once the compiled
# stylesheet is served from /app.css.
fonts:
    mkdir -p src/web/assets/files
    cp -u {{assets_dir}}/node_modules/@fontsource/b612-mono/files/b612-mono-latin-400-normal.* src/web/assets/files/
    cp -u {{assets_dir}}/node_modules/@fontsource/b612-mono/files/b612-mono-latin-700-normal.* src/web/assets/files/

# Lint + test
check:
    cargo check
    cargo test
