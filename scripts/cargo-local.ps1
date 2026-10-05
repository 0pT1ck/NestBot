$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$env:RUSTUP_HOME = Join-Path $taskRoot '.local/tools/rustup'
$env:CARGO_HOME = Join-Path $taskRoot '.local/tools/cargo'
$env:PATH = (Join-Path $env:CARGO_HOME 'bin') + ';' + $env:PATH
$taskCargo = Join-Path $env:CARGO_HOME 'bin/cargo.exe'
$taskCargoArgs = @($args)
# PowerShell can consume the standalone '--' when invoking a script.
if ($taskCargoArgs.Count -gt 1 -and $taskCargoArgs[0] -in @('fmt', 'clippy') -and '--' -notin $taskCargoArgs) {
    for ($taskIndex = 1; $taskIndex -lt $taskCargoArgs.Count; $taskIndex++) {
        if ($taskCargoArgs[$taskIndex] -match '^(--check|-D|-A|-W|-F)$') {
            $taskCargoArgs = @($taskCargoArgs[0..($taskIndex - 1)]) + @('--') + @($taskCargoArgs[$taskIndex..($taskCargoArgs.Count - 1)])
            break
        }
    }
}
Push-Location $taskRoot
try {
    & $taskCargo @taskCargoArgs
    exit $LASTEXITCODE
} finally { Pop-Location }
