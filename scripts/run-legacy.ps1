$ErrorActionPreference = 'Stop'
$taskLegacy = Join-Path (Split-Path -Parent $PSScriptRoot) 'legacy/python'
Push-Location $taskLegacy
try { & '.\.venv\Scripts\python.exe' -m hivesearch @args; exit $LASTEXITCODE }
finally { Pop-Location }
