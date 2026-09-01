param(
    [ValidateSet("Core", "Capture", "All")]
    [string]$Suite = "All",
    [int]$TimeoutSeconds = 90,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$targetRoot = if ($env:CARGO_TARGET_DIR) {
    [System.IO.Path]::GetFullPath($env:CARGO_TARGET_DIR, $repo)
} else {
    Join-Path $repo "target"
}
$executable = Join-Path $targetRoot "debug\demo-win.exe"

$coreModes = @(
    "--scripted",
    "--browser-test",
    "--cookie-test",
    "--profile-test",
    "--incognito-test",
    "--popup-test",
    "--routing-test",
    "--process-test",
    "--download-test",
    "--auth-test",
    "--permission-test",
    "--visibility-test",
    "--find-test",
    "--pdf-test",
    "--context-test",
    "--drop-test",
    "--media-test",
    "--multi-view-test",
    "--keyboard-test",
    "--cdp-input-test",
    "--accelerator-test",
    "--ime-bridge-test"
)
$captureModes = @("--capture-test", "--scale-test")
$modes = switch ($Suite) {
    "Core" { $coreModes }
    "Capture" { $captureModes }
    "All" { $coreModes + $captureModes }
}

if (-not $SkipBuild) {
    Write-Host "==> building demo-win"
    & cargo build --locked -p demo-win
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}

if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "demo-win executable not found at $executable"
}

function Invoke-DemoMode {
    param([string]$Mode)

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $executable
    $startInfo.WorkingDirectory = $repo
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.ArgumentList.Add($Mode)

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "failed to start demo-win $Mode"
    }

    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $timedOut = -not $process.WaitForExit($TimeoutSeconds * 1000)
    if ($timedOut) {
        $process.Kill($true)
        $process.WaitForExit()
    }

    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    if ($stdout) { Write-Host $stdout.TrimEnd() }
    if ($stderr) { [Console]::Error.WriteLine($stderr.TrimEnd()) }

    if ($timedOut) {
        Write-Host "  -> FAIL (timed out after ${TimeoutSeconds}s)"
        return $false
    }
    if ($process.ExitCode -ne 0) {
        Write-Host "  -> FAIL (exit $($process.ExitCode))"
        return $false
    }

    Write-Host "  -> PASS"
    return $true
}

$passed = 0
$failedModes = [System.Collections.Generic.List[string]]::new()
foreach ($mode in $modes) {
    Write-Host ""
    Write-Host "==> $mode"
    if (Invoke-DemoMode -Mode $mode) {
        $passed++
    } else {
        $failedModes.Add($mode)
    }
}

Write-Host ""
Write-Host "==> summary"
Write-Host "  passed: $passed / $($modes.Count)"
Write-Host "  failed: $($failedModes.Count)"
foreach ($mode in $failedModes) {
    Write-Host "    - $mode"
}
if ($failedModes.Count -gt 0) {
    exit 1
}
Write-Host "  all PASS"
