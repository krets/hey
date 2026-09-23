# Wraps the hey binary so it can see the previous command and its exit status.
function hey {
    # Capture status before anything can reset it.
    $__heyOk = $?
    $__heyNative = $global:LASTEXITCODE
    $__heyBin = (Get-Command hey -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    if (-not $__heyBin) {
        Write-Error 'hey: binary not found on PATH'
        return
    }
    $__heyRc = if ($__heyOk) { 0 } elseif ($__heyNative) { $__heyNative } else { 1 }
    $__heyArgs = @('--shell', 'pwsh')
    if ($MyInvocation.ExpectingInput) {
        # `producer | hey`: Get-History cannot see this line yet, so pass the
        # line itself and let the binary pick out the producer.
        $__heyArgs += @('--current-command', $MyInvocation.Line)
        $input | & $__heyBin @__heyArgs @args
    } else {
        $__heyLast = Get-History -Count 1 | ForEach-Object { $_.CommandLine }
        if ($__heyLast) { $__heyArgs += @('--last-command', $__heyLast, '--exit-code', $__heyRc) }
        & $__heyBin @__heyArgs @args
    }
}
