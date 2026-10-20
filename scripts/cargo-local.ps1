# Keep raw arguments, including the -- separator used by clippy/rustc.
$CargoArgs = @($args)
# Windows PowerShell 5.1 consumes a bare -- when invoking a script.
# Restore the clippy-to-rustc separator before lint flags if it was consumed.
if ($CargoArgs.Count -gt 0 -and $CargoArgs[0] -eq 'clippy' -and $CargoArgs -notcontains '--') {
    for ($i = 1; $i -lt $CargoArgs.Count; $i++) {
        if ($CargoArgs[$i] -match '^-[ADFW]') {
            $CargoArgs = @($CargoArgs[0..($i - 1)]) + @('--') + @($CargoArgs[$i..($CargoArgs.Count - 1)])
            break
        }
    }
}
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$env:CARGO_HOME = Join-Path $root 'tmp/cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $root 'target'
$env:TEMP = Join-Path $root 'tmp/build'
$env:TMP = $env:TEMP
$env:CARGO_HTTP_MULTIPLEXING = 'false'
[System.IO.Directory]::CreateDirectory($env:CARGO_HOME) | Out-Null
[System.IO.Directory]::CreateDirectory($env:TEMP) | Out-Null
Push-Location $root
try {
    & cargo @CargoArgs
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}
exit $code
