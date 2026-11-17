#requires -Version 7.0
<#
.SYNOPSIS
    End-to-end test of claude-code-sync on Windows: two simulated PCs sharing
    one git remote, then autosync through Task Scheduler.

.DESCRIPTION
    Everything happens under -WorkDir (default "$env:RUNNER_TEMP\e2e"):

      remote.git            bare git repository, the shared remote
      A\                    PC "A":  C:\Users\Mateo\...-style layout
      B\                    PC "B":  another user, OneDrive-style Documents,
                                     project root mapped with a lowercase
                                     drive letter

    Each PC has its own CLAUDE_CODE_SYNC_CONFIG_DIR / _CLAUDE_DIR / _HOME, so
    the real profile of the machine is never read or written. Transcripts are
    written straight as JSONL, the way Claude Code would.

    Every check prints PASS or FAIL. The first failure stops the run, prints
    diagnostics and exits 1. A final summary table is always printed.

    One check can report GAP (neither PASS nor FAIL): the scheduled task does
    not receive the CLAUDE_CODE_SYNC_* variables. See the autosync section.

.PARAMETER Exe
    Path to claude-code-sync.exe (e.g. target\release\claude-code-sync.exe).

.PARAMETER WorkDir
    Scratch directory; deleted and recreated at the start.

.PARAMETER PollTimeoutSeconds
    How long to wait for the scheduled task to push to the remote.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Exe,

    [string]$WorkDir = $(if ($env:RUNNER_TEMP) { Join-Path $env:RUNNER_TEMP 'e2e' }
                         else { Join-Path ([IO.Path]::GetTempPath()) 'ccs-e2e' }),

    [int]$PollTimeoutSeconds = 120
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------

$script:Results = [System.Collections.Generic.List[object]]::new()
$script:AssertFailed = $false
$script:Failed = $false
$script:Utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$script:TaskName = 'claude-code-sync autosync'   # autosync::WINDOWS_TASK
$script:EnvNames = @('CLAUDE_CODE_SYNC_CONFIG_DIR', 'CLAUDE_CODE_SYNC_CLAUDE_DIR', 'CLAUDE_CODE_SYNC_HOME')
$script:UserEnvSet = $false
$script:AutosyncInstalled = $false

# ---------------------------------------------------------------------------
# Reporting and assertions
# ---------------------------------------------------------------------------

function Write-Result {
    param([string]$Name, [string]$Status, [string]$Detail = '')
    $script:Results.Add([pscustomobject]@{ Check = $Name; Status = $Status; Detail = $Detail })
    $color = switch ($Status) { 'PASS' { 'Green' } 'FAIL' { 'Red' } default { 'Yellow' } }
    $suffix = if ($Detail) { " - $Detail" } else { '' }
    Write-Host "[$Status] $Name$suffix" -ForegroundColor $color
}

function Write-Section {
    param([string]$Title)
    Write-Host ''
    Write-Host "=== $Title ===" -ForegroundColor Cyan
}

function Assert-True {
    param([bool]$Condition, [string]$Name, [string]$Detail = '')
    if ($Condition) {
        Write-Result $Name 'PASS'
    }
    else {
        $script:AssertFailed = $true
        Write-Result $Name 'FAIL' $Detail
        throw "Check failed: $Name. $Detail"
    }
}

function Assert-Equal {
    param($Expected, $Actual, [string]$Name)
    # -ceq: paths, hashes and ids are compared exactly, case included.
    $same = ([string]$Expected) -ceq ([string]$Actual)
    Assert-True $same $Name "expected <$Expected> but got <$Actual>"
}

# ---------------------------------------------------------------------------
# Native commands
# ---------------------------------------------------------------------------

function Invoke-Native {
    <# Runs a native command, capturing stdout+stderr as text (ANSI colors
       stripped). Throws on a non-zero exit unless -AllowFail. #>
    param(
        [Parameter(Mandatory)][string]$File,
        [string[]]$Arguments = @(),
        [switch]$AllowFail
    )
    # stderr of a native command must not become a terminating error.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $lines = & $File @Arguments 2>&1 | ForEach-Object { $_.ToString() }
        $code = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previous
    }
    $text = [regex]::Replace(($lines -join "`n"), '\x1B\[[0-9;]*m', '')
    if ($code -ne 0 -and -not $AllowFail) {
        throw "'$File $($Arguments -join ' ')' exited with $code`n$text"
    }
    [pscustomobject]@{ ExitCode = $code; Output = $text }
}

function Use-Pc {
    <# Points this process's claude-code-sync environment at one PC. #>
    param($Pc)
    $env:CLAUDE_CODE_SYNC_CONFIG_DIR = $Pc.Config
    $env:CLAUDE_CODE_SYNC_CLAUDE_DIR = $Pc.Claude
    $env:CLAUDE_CODE_SYNC_HOME = $Pc.Home
}

function Invoke-Ccs {
    <# Runs claude-code-sync as one PC. #>
    param(
        [Parameter(Mandatory)]$Pc,
        [Parameter(Mandatory)][string[]]$CliArgs,
        [switch]$AllowFail,
        [switch]$Quiet
    )
    Use-Pc $Pc
    Write-Host ("> [{0}] claude-code-sync {1}" -f $Pc.Name, ($CliArgs -join ' ')) -ForegroundColor DarkGray
    $result = Invoke-Native -File $script:ExeFull -Arguments $CliArgs -AllowFail:$AllowFail
    if (-not $Quiet -and $result.Output) {
        foreach ($line in ($result.Output -split "`n")) {
            Write-Host "    $line" -ForegroundColor DarkGray
        }
    }
    $result
}

function Sync-Pc {
    param($Pc)
    [void](Invoke-Ccs -Pc $Pc -CliArgs @('sync'))
}

function Get-RemoteHead {
    $r = Invoke-Native -File 'git' -Arguments @('-C', $script:Remote, 'rev-parse', '--verify', '--quiet', 'refs/heads/main') -AllowFail
    if ($r.ExitCode -ne 0) { return '' }
    $r.Output.Trim()
}

function Get-RemoteCommitCount {
    $r = Invoke-Native -File 'git' -Arguments @('-C', $script:Remote, 'rev-list', '--count', 'refs/heads/main') -AllowFail
    if ($r.ExitCode -ne 0) { return 0 }
    [int]$r.Output.Trim()
}

# ---------------------------------------------------------------------------
# Claude Code naming and transcripts
# ---------------------------------------------------------------------------

function Encode-ClaudePath {
    <# Claude Code's directory name for a project: every character that is
       not [A-Za-z0-9] becomes '-'. (Names over 200 characters are shortened
       and hashed by Claude Code; covered by unit tests, refused here.) #>
    param([Parameter(Mandatory)][string]$Path)
    $encoded = [regex]::Replace($Path, '[^A-Za-z0-9]', '-')
    if ($encoded.Length -gt 200) {
        throw "Path too long for this test (the 200-character rule is not implemented here): $Path"
    }
    $encoded
}

function Get-IsoNow {
    [DateTime]::UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss.fff'Z'", [Globalization.CultureInfo]::InvariantCulture)
}

function ConvertTo-JsonLine {
    param($Object)
    (ConvertTo-Json -InputObject $Object -Compress -Depth 10) + "`n"
}

function New-Turn {
    <# Appends one conversation turn to a transcript: a user entry, the
       assistant's answer and a last-prompt record naming the leaf, as
       UTF-8 without BOM, LF line endings. Returns the leaf (assistant) uuid. #>
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Sid,
        [Parameter(Mandatory)][string]$Cwd,
        [Parameter(Mandatory)][string]$Text,
        $ParentUuid = $null
    )
    $userId = [guid]::NewGuid().ToString()
    $assistantId = [guid]::NewGuid().ToString()

    $user = [ordered]@{
        type       = 'user'
        uuid       = $userId
        parentUuid = $ParentUuid
        sessionId  = $Sid
        cwd        = $Cwd
        timestamp  = Get-IsoNow
        message    = [ordered]@{ role = 'user'; content = $Text }
    }
    $assistant = [ordered]@{
        type       = 'assistant'
        uuid       = $assistantId
        parentUuid = $userId
        sessionId  = $Sid
        cwd        = $Cwd
        timestamp  = Get-IsoNow
        message    = [ordered]@{
            role    = 'assistant'
            content = @([ordered]@{ type = 'text'; text = 'ok' })
        }
    }
    $lastPrompt = [ordered]@{ type = 'last-prompt'; leafUuid = $assistantId; sessionId = $Sid }

    $text = (ConvertTo-JsonLine $user) + (ConvertTo-JsonLine $assistant) + (ConvertTo-JsonLine $lastPrompt)
    [void](New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path))
    [IO.File]::AppendAllText($Path, $text, $script:Utf8NoBom)
    $assistantId
}

function Get-LeafUuid {
    <# The leaf named by the transcript's last last-prompt record. #>
    param([Parameter(Mandatory)][string]$Path)
    $leaf = $null
    foreach ($line in [IO.File]::ReadAllLines($Path)) {
        if ($line.Contains('"last-prompt"')) {
            $leaf = (ConvertFrom-Json -InputObject $line).leafUuid
        }
    }
    if (-not $leaf) { throw "No last-prompt record in $Path" }
    $leaf
}

function Get-Hash {
    param([Parameter(Mandatory)][string]$Path)
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash
}

function Get-Snapshot {
    <# Every file under a PC's claude\projects, as sorted "relative|sha256" lines. #>
    param($Pc)
    $root = Join-Path $Pc.Claude 'projects'
    $rows = Get-ChildItem -LiteralPath $root -Recurse -File | ForEach-Object {
        $relative = $_.FullName.Substring($root.Length).TrimStart('\')
        "$relative|$(Get-Hash $_.FullName)"
    }
    (@($rows) | Sort-Object) -join "`n"
}

# ---------------------------------------------------------------------------
# The simulated PCs
# ---------------------------------------------------------------------------

function New-Pc {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$UserRel,      # profile folder, relative to the PC's drive root
        [Parameter(Mandatory)][string]$RootRel,      # the projects folder, same base
        [switch]$LowercaseDriveInMap
    )
    $base = Join-Path $WorkDir $Name
    $root = Join-Path $base $RootRel
    $rootCfg = $root
    if ($LowercaseDriveInMap -and $root -match '^[A-Za-z]:') {
        $rootCfg = $root.Substring(0, 1).ToLowerInvariant() + $root.Substring(1)
    }
    $pc = [pscustomobject]@{
        Name    = $Name
        Home    = Join-Path $base $UserRel
        Root    = $root          # the real path, as Claude Code records it in `cwd`
        RootCfg = $rootCfg       # the spelling given to --map-root
        Claude  = Join-Path $base 'claude'
        Config  = Join-Path $base 'cfg'
        Repo    = Join-Path $base 'sync-repo'
    }
    foreach ($dir in @($pc.Home, $pc.Root, (Join-Path $pc.Claude 'projects'), $pc.Config)) {
        [void](New-Item -ItemType Directory -Force -Path $dir)
    }
    $pc
}

function Get-ProjectCwd {
    param($Pc, [string]$Rel)
    Join-Path $Pc.Root $Rel
}

function Get-TranscriptDir {
    <# Where this PC's Claude Code keeps a project's transcripts, named from
       the root as this PC's map spells it (the tool materializes it so). #>
    param($Pc, [string]$Rel)
    Join-Path (Join-Path $Pc.Claude 'projects') (Encode-ClaudePath (Join-Path $Pc.RootCfg $Rel))
}

function Get-Transcript {
    param($Pc, [string]$Rel, [string]$Sid)
    Join-Path (Get-TranscriptDir $Pc $Rel) "$Sid.jsonl"
}

function Get-JsonlHashes {
    <# name -> sha256 of every transcript of a project, as one comparable string. #>
    param($Pc, [string]$Rel)
    $rows = Get-ChildItem -LiteralPath (Get-TranscriptDir $Pc $Rel) -Filter '*.jsonl' -File |
        ForEach-Object { "$($_.Name)|$(Get-Hash $_.FullName)" }
    (@($rows) | Sort-Object) -join "`n"
}

# ---------------------------------------------------------------------------
# settings.json helpers (autosync)
# ---------------------------------------------------------------------------

function Read-SettingsJson {
    <# Parses settings.json, failing on a BOM or invalid UTF-8/JSON. #>
    param([Parameter(Mandatory)][string]$Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    $hasBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
    Assert-True (-not $hasBom) 'settings.json has no UTF-8 BOM'
    $strict = [System.Text.UTF8Encoding]::new($false, $true)
    $json = $strict.GetString($bytes)
    $parsed = ConvertFrom-Json -InputObject $json -AsHashtable
    Assert-True ($parsed -is [System.Collections.IDictionary]) 'settings.json is a valid JSON object'
    $parsed
}

function Get-SessionEndCommands {
    param([System.Collections.IDictionary]$Settings)
    $commands = [System.Collections.Generic.List[string]]::new()
    if ($Settings.Contains('hooks') -and $Settings['hooks'].Contains('SessionEnd')) {
        foreach ($group in @($Settings['hooks']['SessionEnd'])) {
            foreach ($hook in @($group['hooks'])) {
                # Exec form (`args` set) runs `command` directly with those
                # arguments; shell form keeps everything in `command`.
                $line = [string]$hook['command']
                if ($hook.Contains('args')) {
                    $line = (@($line) + @($hook['args'] | ForEach-Object { [string]$_ })) -join ' '
                }
                $commands.Add($line)
            }
        }
    }
    , $commands.ToArray()
}

function Test-OurHook {
    param([string]$Command)
    $normalized = $Command -replace '\\', '/'
    $normalized.EndsWith('autosync run --detach', [StringComparison]::Ordinal) -and
    $normalized.IndexOf(($script:ExeFull -replace '\\', '/'), [StringComparison]::OrdinalIgnoreCase) -ge 0
}

function Test-TaskExists {
    $r = Invoke-Native -File 'schtasks' -Arguments @('/Query', '/TN', $script:TaskName) -AllowFail
    $r.ExitCode -eq 0
}

function Set-UserEnvironment {
    param($Pc)
    [Environment]::SetEnvironmentVariable('CLAUDE_CODE_SYNC_CONFIG_DIR', $Pc.Config, 'User')
    [Environment]::SetEnvironmentVariable('CLAUDE_CODE_SYNC_CLAUDE_DIR', $Pc.Claude, 'User')
    [Environment]::SetEnvironmentVariable('CLAUDE_CODE_SYNC_HOME', $Pc.Home, 'User')
    $script:UserEnvSet = $true
}

function Clear-UserEnvironment {
    foreach ($name in $script:EnvNames) {
        [Environment]::SetEnvironmentVariable($name, $null, 'User')
    }
    $script:UserEnvSet = $false
}

function Write-Diagnostics {
    Write-Host ''
    Write-Host '--- diagnostics ---' -ForegroundColor Yellow
    if (Test-Path -LiteralPath $WorkDir) {
        Get-ChildItem -LiteralPath $WorkDir -Recurse -Filter 'autosync.log' -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "autosync.log: $($_.FullName)" -ForegroundColor Yellow
            Get-Content -LiteralPath $_.FullName -Tail 60 | ForEach-Object { Write-Host "    $_" }
        }
    }
    $q = Invoke-Native -File 'schtasks' -Arguments @('/Query', '/TN', $script:TaskName, '/V', '/FO', 'LIST') -AllowFail
    Write-Host "schtasks /Query /V (exit $($q.ExitCode)):" -ForegroundColor Yellow
    Write-Host $q.Output
}

# ===========================================================================
# Main
# ===========================================================================

try {
    # -----------------------------------------------------------------------
    Write-Section 'Prerequisites'
    Assert-True ([bool]$IsWindows) 'running on Windows' 'this test needs Windows (schtasks, drive letters)'
    Assert-True (Test-Path -LiteralPath $Exe -PathType Leaf) "binary exists: $Exe"
    $script:ExeFull = (Resolve-Path -LiteralPath $Exe).Path
    $version = Invoke-Native -File $script:ExeFull -Arguments @('--version')
    Write-Host "    $($version.Output)" -ForegroundColor DarkGray
    $gitVersion = Invoke-Native -File 'git' -Arguments @('--version')
    Write-Host "    $($gitVersion.Output)" -ForegroundColor DarkGray

    # The sync repositories and the bare remote must all use branch "main".
    # Process-scoped (not `git config --global`), so a developer's own git
    # configuration is untouched when this is run by hand.
    $env:GIT_CONFIG_COUNT = '1'
    $env:GIT_CONFIG_KEY_0 = 'init.defaultBranch'
    $env:GIT_CONFIG_VALUE_0 = 'main'

    # -----------------------------------------------------------------------
    Write-Section 'Workspace'
    if (Test-Path -LiteralPath $WorkDir) { Remove-Item -LiteralPath $WorkDir -Recurse -Force }
    [void](New-Item -ItemType Directory -Force -Path $WorkDir)
    $script:Remote = Join-Path $WorkDir 'remote.git'
    [void](Invoke-Native -File 'git' -Arguments @('init', '--bare', '--quiet', $script:Remote))
    Assert-True (Test-Path -LiteralPath (Join-Path $script:Remote 'HEAD')) "bare remote created: $script:Remote"

    $A = New-Pc -Name 'A' -UserRel 'Users\Mateo' -RootRel 'Users\Mateo\Documents\proyectos'
    $B = New-Pc -Name 'B' -UserRel 'Users\mateo.bodenlle' `
        -RootRel 'Users\mateo.bodenlle\OneDrive\Documentos\proyectos' -LowercaseDriveInMap
    Write-Host "    A root: $($A.Root)  (map: $($A.RootCfg))" -ForegroundColor DarkGray
    Write-Host "    B root: $($B.Root)  (map: $($B.RootCfg))" -ForegroundColor DarkGray

    $projects = @(
        [pscustomobject]@{ Rel = 'jarvis'; Sid = [guid]::NewGuid().ToString() },
        [pscustomobject]@{ Rel = 'osix\web_app.v2'; Sid = [guid]::NewGuid().ToString() }
    )
    $jarvis = $projects[0]

    # The project folders themselves exist on both PCs (Syncthing does that in real life).
    foreach ($pc in @($A, $B)) {
        foreach ($p in $projects) {
            [void](New-Item -ItemType Directory -Force -Path (Get-ProjectCwd $pc $p.Rel))
        }
    }
    Assert-True (Test-Path -LiteralPath (Get-ProjectCwd $B 'osix\web_app.v2') -PathType Container) 'project folders exist on both PCs'

    # -----------------------------------------------------------------------
    Write-Section 'PC A: create two transcripts'
    foreach ($p in $projects) {
        $path = Get-Transcript $A $p.Rel $p.Sid
        [void](New-Turn -Path $path -Sid $p.Sid -Cwd (Get-ProjectCwd $A $p.Rel) -Text 'hola desde A')
        Assert-True (Test-Path -LiteralPath $path -PathType Leaf) "A transcript written: $($p.Rel)"
    }
    Assert-Equal 'C--Users-Mateo-Documents-proyectos-jarvis' `
        (Encode-ClaudePath 'C:\Users\Mateo\Documents\proyectos\jarvis') 'Encode-ClaudePath matches the documented example'

    # -----------------------------------------------------------------------
    Write-Section 'PC A: init, map-root, sync'
    [void](Invoke-Ccs -Pc $A -CliArgs @('init', '--local', $A.Repo, '--remote', $script:Remote))
    [void](Invoke-Ccs -Pc $A -CliArgs @('config', '--map-root', "proyectos=$($A.RootCfg)", '--auto-home-root', 'false'))
    Sync-Pc $A
    Assert-True ((Get-RemoteCommitCount) -ge 1) 'A pushed to the remote (refs/heads/main exists)'

    # -----------------------------------------------------------------------
    Write-Section 'PC B: init --clone, map-root (lowercase drive), sync'
    [void](Invoke-Ccs -Pc $B -CliArgs @('init', '--local', $B.Repo, '--remote', $script:Remote, '--clone'))
    [void](Invoke-Ccs -Pc $B -CliArgs @('config', '--map-root', "proyectos=$($B.RootCfg)", '--auto-home-root', 'false'))
    Sync-Pc $B

    foreach ($p in $projects) {
        $onA = Get-Transcript $A $p.Rel $p.Sid
        $onB = Get-Transcript $B $p.Rel $p.Sid
        Assert-True (Test-Path -LiteralPath $onB -PathType Leaf) "B has $($p.Rel) at its own encoded directory"
        Assert-Equal (Get-Hash $onA) (Get-Hash $onB) "B's $($p.Rel) is byte-identical to A's"
    }
    $bDirs = @(Get-ChildItem -LiteralPath (Join-Path $B.Claude 'projects') -Directory)
    Assert-Equal 2 $bDirs.Count 'B has exactly two project directories (no strays)'
    $wanted = Encode-ClaudePath (Join-Path $B.RootCfg 'jarvis')
    Assert-True ($bDirs.Name -ccontains $wanted) `
        "B's jarvis directory follows the root as configured ($wanted)" "found: $($bDirs.Name -join ', ')"
    if ($B.Root -cne $B.RootCfg) {
        Write-Result 'drive-letter case differs between B real path and B map' 'PASS' "$($B.Root.Substring(0, 2)) vs $($B.RootCfg.Substring(0, 2))"
    }

    # -----------------------------------------------------------------------
    Write-Section 'B continues jarvis: B sync, A sync (fast-forward)'
    $bJarvis = Get-Transcript $B $jarvis.Rel $jarvis.Sid
    # B's cwd keeps the drive letter's real (upper) case; its directory was
    # materialized with the lowercase spelling of the map. Same folder on NTFS.
    [void](New-Turn -Path $bJarvis -Sid $jarvis.Sid -Cwd (Get-ProjectCwd $B $jarvis.Rel) `
            -Text 'desde B (turno 2)' -ParentUuid (Get-LeafUuid $bJarvis))
    Sync-Pc $B
    Sync-Pc $A
    $aJarvis = Get-Transcript $A $jarvis.Rel $jarvis.Sid
    Assert-Equal (Get-Hash $bJarvis) (Get-Hash $aJarvis) "A's jarvis now equals B's (fast-forward)"
    Assert-True ([IO.File]::ReadAllText($aJarvis).Contains('desde B (turno 2)')) "A's jarvis holds B's turn"
    $aJarvisFiles = @(Get-ChildItem -LiteralPath (Get-TranscriptDir $A $jarvis.Rel) -Filter '*.jsonl' -File)
    Assert-Equal 1 $aJarvisFiles.Count 'no parallel branch for a fast-forward'

    # -----------------------------------------------------------------------
    Write-Section 'Divergence: A and B both continue jarvis without syncing'
    $leaf = Get-LeafUuid $aJarvis
    [void](New-Turn -Path $aJarvis -Sid $jarvis.Sid -Cwd (Get-ProjectCwd $A $jarvis.Rel) `
            -Text 'turno-divergente-A' -ParentUuid $leaf)
    Start-Sleep -Milliseconds 50
    [void](New-Turn -Path $bJarvis -Sid $jarvis.Sid -Cwd (Get-ProjectCwd $B $jarvis.Rel) `
            -Text 'turno-divergente-B' -ParentUuid $leaf)
    Sync-Pc $B
    Sync-Pc $A
    Sync-Pc $B
    Sync-Pc $A

    $dirA = Get-TranscriptDir $A $jarvis.Rel
    $dirB = Get-TranscriptDir $B $jarvis.Rel
    $filesA = @(Get-ChildItem -LiteralPath $dirA -Filter '*.jsonl' -File)
    $filesB = @(Get-ChildItem -LiteralPath $dirB -Filter '*.jsonl' -File)
    Assert-Equal 2 $filesA.Count 'A has two transcripts in jarvis after the split'
    Assert-Equal 2 $filesB.Count 'B has two transcripts in jarvis after the split'
    Assert-Equal (Get-JsonlHashes $A $jarvis.Rel) (Get-JsonlHashes $B $jarvis.Rel) 'both PCs hold byte-identical files in jarvis'

    $originalName = "$($jarvis.Sid).jsonl"
    Assert-True ($filesA.Name -ccontains $originalName) 'the original session id is still there'
    $branchFile = $filesA | Where-Object { $_.Name -cne $originalName } | Select-Object -First 1
    $branchId = [IO.Path]::GetFileNameWithoutExtension($branchFile.Name)
    $parsedGuid = [guid]::Empty
    Assert-True ([guid]::TryParse($branchId, [ref]$parsedGuid)) "the parallel branch has a new session id ($branchId)"

    $branchLines = @([IO.File]::ReadAllLines($branchFile.FullName) | Where-Object { $_.Trim() })
    $lastEntry = ConvertFrom-Json -InputObject $branchLines[-1]
    Assert-Equal 'custom-title' $lastEntry.type "the branch's last line is a custom-title"
    Assert-True ($lastEntry.customTitle -like '*(rama paralela)*') `
        'its title says (rama paralela)' "customTitle = $($lastEntry.customTitle)"
    Assert-Equal $branchId $lastEntry.sessionId "the branch's custom-title carries the new session id"

    $original = [IO.File]::ReadAllText((Join-Path $dirA $originalName))
    $branch = [IO.File]::ReadAllText($branchFile.FullName)
    $origA = $original.Contains('turno-divergente-A'); $origB = $original.Contains('turno-divergente-B')
    $brA = $branch.Contains('turno-divergente-A'); $brB = $branch.Contains('turno-divergente-B')
    Assert-True (($origA -ne $origB) -and ($brA -ne $brB) -and ($origA -ne $brA)) `
        'each divergent turn lives in exactly one of the two files, none lost' `
        "original: A=$origA B=$origB; branch: A=$brA B=$brB"
    Assert-True (-not $original.Contains('(rama paralela)')) 'the original session carries no branch title'

    # -----------------------------------------------------------------------
    Write-Section 'Converged: another sync changes nothing'
    $headBefore = Get-RemoteHead
    $snapA = Get-Snapshot $A
    $snapB = Get-Snapshot $B
    Sync-Pc $A
    Sync-Pc $B
    Assert-Equal $snapA (Get-Snapshot $A) 'A: file hashes unchanged by a further sync'
    Assert-Equal $snapB (Get-Snapshot $B) 'B: file hashes unchanged by a further sync'
    Assert-Equal $headBefore (Get-RemoteHead) 'remote main unchanged by a further sync'

    # -----------------------------------------------------------------------
    Write-Section 'Autosync on A (Task Scheduler)'
    $settingsPath = Join-Path $A.Claude 'settings.json'
    $seed = @'
{
  "cleanupPeriodDays": 30,
  "model": "opus",
  "hooks": {
    "SessionEnd": [
      { "hooks": [ { "type": "command", "command": "echo keep-me" } ] }
    ]
  }
}
'@
    [IO.File]::WriteAllText($settingsPath, $seed, $script:Utf8NoBom)

    Assert-True (-not (Test-TaskExists)) 'no autosync task exists before install'
    $script:AutosyncInstalled = $true
    [void](Invoke-Ccs -Pc $A -CliArgs @('autosync', 'install', '--interval-minutes', '1'))

    Assert-True (Test-TaskExists) "schtasks /Query finds the task ""$script:TaskName"""
    $xml = Invoke-Native -File 'schtasks' -Arguments @('/Query', '/TN', $script:TaskName, '/XML') -AllowFail
    if ($xml.Output -match '<Interval>PT1M</Interval>') {
        Write-Result 'task repeats every 1 minute (XML Interval PT1M)' 'PASS'
    }
    else {
        Write-Result 'task repeats every 1 minute (XML Interval PT1M)' 'INFO' 'not found in the task XML; check the dump below'
        Write-Host $xml.Output -ForegroundColor DarkGray
    }

    $settings = Read-SettingsJson $settingsPath
    Assert-True ([int64]$settings['cleanupPeriodDays'] -ge 3650) 'settings.json: cleanupPeriodDays >= 3650 (was 30)' "got $($settings['cleanupPeriodDays'])"
    Assert-Equal 'opus' $settings['model'] 'settings.json: unrelated members are kept'
    $commands = Get-SessionEndCommands $settings
    Assert-True ($commands -contains 'echo keep-me') 'settings.json: the existing SessionEnd hook is kept'
    $ours = @($commands | Where-Object { Test-OurHook $_ })
    Assert-Equal 1 $ours.Count "settings.json: exactly one SessionEnd hook points at the exe ($($ours -join ''))"

    $vbsPath = Join-Path $A.Config 'claude-code-sync\autosync.vbs'
    Assert-True (Test-Path -LiteralPath $vbsPath -PathType Leaf) 'the hidden .vbs wrapper was written'
    $vbsBytes = [IO.File]::ReadAllBytes($vbsPath)
    Assert-True ($vbsBytes.Length -gt 2 -and $vbsBytes[0] -eq 0xFF -and $vbsBytes[1] -eq 0xFE) 'the .vbs is UTF-16LE with a BOM'
    $vbsText = [System.Text.Encoding]::Unicode.GetString($vbsBytes, 2, $vbsBytes.Length - 2)
    Assert-True ($vbsText.IndexOf($script:ExeFull, [StringComparison]::OrdinalIgnoreCase) -ge 0 -and
        $vbsText.Contains('autosync run --detach')) 'the .vbs runs "<exe> autosync run --detach"'

    # Does the scheduled run get the isolated configuration? Task Scheduler
    # starts the task from the user's registered environment, not from this
    # process. The launchd/systemd units get PASSED_THROUGH_ENV written into
    # them; the Windows .vbs is written without it (autosync.rs: windows_vbs).
    $passesEnv = $vbsText.Contains('CLAUDE_CODE_SYNC_CONFIG_DIR')
    if ($passesEnv) {
        Write-Result 'task receives CLAUDE_CODE_SYNC_* (written into the .vbs)' 'PASS'
    }
    else {
        $gap = 'autosync.vbs sets no environment, so the scheduled run falls back to %APPDATA% and the real ~/.claude, ' +
        'not to CLAUDE_CODE_SYNC_CONFIG_DIR/_CLAUDE_DIR/_HOME (launchd and systemd units do carry them). ' +
        'This test sets them at User scope so the task can see them; that is a harness workaround, not product behavior.'
        Write-Result 'task receives CLAUDE_CODE_SYNC_* (written into the .vbs)' 'GAP' $gap
        if ($env:GITHUB_ACTIONS) { Write-Host "::warning title=autosync on Windows::$gap" }
        Set-UserEnvironment $A
    }

    $status = Invoke-Ccs -Pc $A -CliArgs @('autosync', 'status')
    Assert-True ($status.Output -match 'Task Scheduler task "claude-code-sync autosync" \(present\)') 'autosync status: task present'
    Assert-True ($status.Output -match 'SessionEnd hook: installed') 'autosync status: hook installed'
    Assert-True ($status.Output -match 'retention: 3650 days') 'autosync status: retention 3650 days'

    # A new local change on A, then run the task right away.
    $autoMarker = 'turno-por-autosync'
    $remoteBefore = Get-RemoteHead
    $countBefore = Get-RemoteCommitCount
    [void](New-Turn -Path $aJarvis -Sid $jarvis.Sid -Cwd (Get-ProjectCwd $A $jarvis.Rel) `
            -Text $autoMarker -ParentUuid (Get-LeafUuid $aJarvis))
    Write-Host "    remote main before: $remoteBefore ($countBefore commits)" -ForegroundColor DarkGray
    [void](Invoke-Native -File 'schtasks' -Arguments @('/Run', '/TN', $script:TaskName))

    $deadline = [DateTime]::UtcNow.AddSeconds($PollTimeoutSeconds)
    $started = [DateTime]::UtcNow
    $arrived = $false
    while ([DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Seconds 3
        if ((Get-RemoteHead) -ne $remoteBefore) { $arrived = $true; break }
    }
    $elapsed = [int]([DateTime]::UtcNow - $started).TotalSeconds
    if (-not $arrived) {
        Write-Diagnostics
    }
    Assert-True $arrived "the scheduled task pushed a new commit to the remote (waited up to ${PollTimeoutSeconds}s)" `
        'no new commit on remote main; if the GAP above was reported, the task may not see the test configuration'
    Write-Host "    arrived after ~${elapsed}s: $(Get-RemoteHead)" -ForegroundColor DarkGray
    Assert-True ((Get-RemoteCommitCount) -gt $countBefore) 'remote main has more commits than before'

    $remoteFile = Invoke-Native -File 'git' -Arguments @('-C', $script:Remote, 'show', "refs/heads/main:projects/@proyectos@jarvis/$($jarvis.Sid).jsonl")
    Assert-True ($remoteFile.Output.Contains($autoMarker)) "the pushed jarvis transcript holds A's new turn"

    $logPath = Join-Path $A.Config 'claude-code-sync\autosync.log'
    if (Test-Path -LiteralPath $logPath) {
        Write-Host "    autosync.log (tail):" -ForegroundColor DarkGray
        Get-Content -LiteralPath $logPath -Tail 8 | ForEach-Object { Write-Host "      $_" -ForegroundColor DarkGray }
    }

    # Uninstall.
    [void](Invoke-Ccs -Pc $A -CliArgs @('autosync', 'uninstall'))
    $script:AutosyncInstalled = $false
    Assert-True (-not (Test-TaskExists)) 'autosync uninstall: the scheduled task is gone'
    Assert-True (-not (Test-Path -LiteralPath $vbsPath)) 'autosync uninstall: the .vbs wrapper is removed'
    $after = Read-SettingsJson $settingsPath
    $afterCommands = Get-SessionEndCommands $after
    $oursAfter = @($afterCommands | Where-Object { Test-OurHook $_ })
    Assert-Equal 0 $oursAfter.Count 'autosync uninstall: our SessionEnd hook is removed'
    Assert-True ($afterCommands -contains 'echo keep-me') 'autosync uninstall: the other hook stays'
    Assert-True ([int64]$after['cleanupPeriodDays'] -ge 3650) 'autosync uninstall: cleanupPeriodDays stays >= 3650' "got $($after['cleanupPeriodDays'])"
    Assert-Equal 'opus' $after['model'] 'autosync uninstall: unrelated members still kept'
    $statusAfter = Invoke-Ccs -Pc $A -CliArgs @('autosync', 'status')
    Assert-True ($statusAfter.Output -match 'Timer: not installed') 'autosync status after uninstall: not installed'
}
catch {
    if (-not $script:AssertFailed) {
        Write-Result 'unexpected error' 'FAIL' ($_.Exception.Message -split "`n" | Select-Object -First 1)
    }
    Write-Host ''
    Write-Host $_.Exception.Message -ForegroundColor Red
    if ($_.ScriptStackTrace) { Write-Host $_.ScriptStackTrace -ForegroundColor DarkGray }
    $script:Failed = $true
    try { Write-Diagnostics } catch { Write-Host "diagnostics failed: $($_.Exception.Message)" }
}
finally {
    # Leave nothing scheduled and no user-level variables behind.
    try {
        if ($script:AutosyncInstalled -and (Test-TaskExists)) {
            [void](Invoke-Native -File 'schtasks' -Arguments @('/Delete', '/TN', $script:TaskName, '/F') -AllowFail)
        }
        if ($script:UserEnvSet) { Clear-UserEnvironment }
    }
    catch { Write-Host "cleanup failed: $($_.Exception.Message)" -ForegroundColor Yellow }
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

Write-Host ''
Write-Host '=== Summary ===' -ForegroundColor Cyan
$table = $script:Results | Format-Table -Wrap -AutoSize Status, Check, Detail | Out-String -Width 200
Write-Host $table

$pass = @($script:Results | Where-Object Status -eq 'PASS').Count
$fail = @($script:Results | Where-Object Status -eq 'FAIL').Count
$other = @($script:Results | Where-Object { $_.Status -notin 'PASS', 'FAIL' }).Count
Write-Host "PASS: $pass   FAIL: $fail   GAP/INFO: $other"

if ($env:GITHUB_STEP_SUMMARY) {
    $md = [System.Text.StringBuilder]::new()
    [void]$md.AppendLine('## claude-code-sync Windows e2e')
    [void]$md.AppendLine('')
    [void]$md.AppendLine('| Status | Check | Detail |')
    [void]$md.AppendLine('|---|---|---|')
    foreach ($r in $script:Results) {
        $detail = ($r.Detail -replace '\|', '\|' -replace '\r?\n', ' ')
        [void]$md.AppendLine("| $($r.Status) | $($r.Check -replace '\|', '\|') | $detail |")
    }
    [System.IO.File]::AppendAllText($env:GITHUB_STEP_SUMMARY, $md.ToString(), $script:Utf8NoBom)
}

if ($fail -gt 0 -or $script:Failed) { exit 1 }
exit 0
