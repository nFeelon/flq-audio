# Build the flq wasm decoder into web/flq_wasm.wasm
cargo build --release --target wasm32-unknown-unknown --manifest-path "$PSScriptRoot\Cargo.toml"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force "$PSScriptRoot\..\web" | Out-Null
Copy-Item "$PSScriptRoot\target\wasm32-unknown-unknown\release\flq_wasm.wasm" "$PSScriptRoot\..\web\flq_wasm.wasm" -Force
Write-Output "wrote web\flq_wasm.wasm"
