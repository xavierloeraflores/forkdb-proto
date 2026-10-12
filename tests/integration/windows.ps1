param(
    [string]$Binary = (Join-Path $PSScriptRoot '..\..\target\release\dbfork-proto.exe')
)

$ErrorActionPreference = 'Stop'
$project = Join-Path ([IO.Path]::GetTempPath()) ("dbfork project %20 # ' quote " + [guid]::NewGuid().ToString('N'))
$previousDatabaseUrl = [Environment]::GetEnvironmentVariable('DATABASE_URL')
New-Item -ItemType Directory -Path $project | Out-Null

function Invoke-Dbfork {
    param([string[]]$Arguments)
    $output = & $Binary -C $project --json @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "dbfork-proto $($Arguments -join ' ') failed: $output"
    }
    $response = $output | ConvertFrom-Json
    if (-not $response.ok) {
        throw "dbfork-proto $($Arguments -join ' ') returned an error: $output"
    }
    return $response.result
}

try {
    Remove-Item Env:DATABASE_URL -ErrorAction SilentlyContinue
    Invoke-Dbfork -Arguments @('init') | Out-Null
    Invoke-Dbfork -Arguments @('create', 'feat/windows') | Out-Null
    $status = Invoke-Dbfork -Arguments @('status', '--check')
    if ($status.environment -ne 'feat/windows' -or -not $status.health[0].healthy) {
        throw 'full clone was not activated and healthy'
    }

    Invoke-Dbfork -Arguments @('create', 'feat/child', '--from', 'feat/windows', '--no-switch') | Out-Null
    Invoke-Dbfork -Arguments @('create', 'feat/schema', '--schema-only', '--no-switch') | Out-Null

    $configPath = Join-Path $project '.dbfork.toml'
    $config = [IO.File]::ReadAllText($configPath)
    if (-not $config.Contains('seed = []')) {
        throw 'init did not create an editable seed list'
    }
    [IO.File]::WriteAllText($configPath, $config.Replace('seed = []', 'seed = ["seed.sql"]'))
    [IO.File]::WriteAllText((Join-Path $project 'seed.sql'), 'CREATE TABLE seeded_marker(value TEXT); INSERT INTO seeded_marker VALUES (''ok'');')
    Invoke-Dbfork -Arguments @('create', 'feat/seeded', '--strategy', 'seeded', '--no-switch') | Out-Null

    $list = Invoke-Dbfork -Arguments @('list')
    if ($list.environments.Count -ne 4) {
        throw "expected four disposable environments, got $($list.environments.Count)"
    }

    Invoke-Dbfork -Arguments @('switch', 'main') | Out-Null
    foreach ($branch in @('feat/seeded', 'feat/schema', 'feat/child', 'feat/windows')) {
        Invoke-Dbfork -Arguments @('delete', $branch, '--yes') | Out-Null
    }
    $status = Invoke-Dbfork -Arguments @('status', '--check')
    if ($status.environment -ne 'main' -or $status.environments -ne 0 -or -not $status.health[0].healthy) {
        throw 'source was not restored and healthy after deleting clones'
    }
    Write-Host 'Windows release CLI lifecycle passed.'
}
finally {
    [Environment]::SetEnvironmentVariable('DATABASE_URL', $previousDatabaseUrl)
    Remove-Item -LiteralPath $project -Recurse -Force
}
