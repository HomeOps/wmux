<#
.SYNOPSIS
    Rotates the token used by wmux's release workflow, entirely from the CLI.

.DESCRIPTION
    Stores one GitHub token in the WINGET_TOKEN and RELEASE_PLEASE_TOKEN
    repository secrets, after checking who owns it and that it has the scopes
    the release needs:

      public_repo (or repo)  release-please PRs, auto-merge, winget-pkgs PR
      workflow               syncing the winget-pkgs fork, whose upstream
                             changes its own workflow files

    GitHub has no API for creating personal access tokens, so the token has
    to come from somewhere: either paste a PAT you created earlier (it is read
    without echo), or pass -UseGhCliToken to reuse the token of the current
    `gh` login. The token is handed to `gh secret set` on stdin, so it never
    appears on a command line, in history, or in a log.

.PARAMETER Repository
    The repository whose secrets are set. Defaults to HomeOps/wmux.

.PARAMETER SecretName
    The secrets to set. Defaults to WINGET_TOKEN and RELEASE_PLEASE_TOKEN.

.PARAMETER NoBrowser
    Print the token-creation URL instead of opening it. When prompting for a
    token the script otherwise opens GitHub's classic-PAT page with the
    public_repo and workflow scopes pre-selected.

.PARAMETER UseGhCliToken
    Use the token of the current `gh` login instead of prompting. Fully
    non-interactive, but that token usually carries more scopes than the
    release needs, and stops working after `gh auth logout`.

.EXAMPLE
    .\Update-ReleaseToken.ps1
    Prompts for a PAT, validates it, and stores it in both secrets.

.EXAMPLE
    .\Update-ReleaseToken.ps1 -UseGhCliToken -WhatIf
    Shows what would be set using the gh login's token, without setting it.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidatePattern('^[\w.-]+/[\w.-]+$')]
    [string]$Repository = 'HomeOps/wmux',

    [ValidateNotNullOrEmpty()]
    [string[]]$SecretName = @('WINGET_TOKEN', 'RELEASE_PLEASE_TOKEN'),

    [switch]$UseGhCliToken,

    [switch]$NoBrowser
)

# Classic PAT form with the two scopes the release needs already ticked.
# Fine-grained tokens do not work with winget-releaser. GitHub requires each
# token's note to be unique ("Note has already been taken"), and has no API to
# delete the previous token first, so every rotation gets a timestamped note.
$tokenNote = "wmux-release-$(Get-Date -Format 'yyyy-MM-dd-HHmm')"
$tokenPage = "https://github.com/settings/tokens/new?scopes=public_repo,workflow&description=$tokenNote"

$ErrorActionPreference = 'Stop'
$activity = "Rotating release token for $Repository"

$gh = Get-Command gh -CommandType Application -ErrorAction SilentlyContinue
if (-not $gh) {
    Write-Error 'The GitHub CLI (gh) is not on PATH. Install it with: winget install GitHub.cli'
    return
}
Write-Verbose "Using gh at '$($gh.Source)'."

Write-Progress -Activity $activity -Status 'Checking gh login' -PercentComplete 10
& $gh auth status --hostname github.com *> $null
if ($LASTEXITCODE -ne 0) {
    Write-Error 'gh is not logged in to github.com. Run: gh auth login'
    return
}

# --- obtain the token --------------------------------------------------------
Write-Progress -Activity $activity -Status 'Reading token' -PercentComplete 20
if ($UseGhCliToken) {
    Write-Verbose 'Taking the token from the current gh login.'
    $plain = (& $gh auth token --hostname github.com).Trim()
    if (-not $plain) {
        Write-Error 'gh auth token returned nothing.'
        return
    }
} else {
    if ($NoBrowser) {
        Write-Verbose 'Not opening the browser (-NoBrowser).'
        Write-Host "Create a classic token with public_repo and workflow at: $tokenPage"
    } else {
        Write-Verbose "Opening $tokenPage"
        Write-Host 'Opening GitHub with public_repo and workflow pre-selected. Set an expiry, generate the token, and paste it below.'
        Start-Process $tokenPage
    }
    $secure = Read-Host -AsSecureString -Prompt 'Paste the new GitHub token (input is hidden)'
    $plain = [System.Net.NetworkCredential]::new('', $secure).Password.Trim()
    if (-not $plain) {
        Write-Error 'No token entered.'
        return
    }
}

try {
    # --- validate it -----------------------------------------------------------
    Write-Progress -Activity $activity -Status 'Validating token' -PercentComplete 40
    Write-Verbose 'Calling GET https://api.github.com/user with the token.'
    try {
        $response = Invoke-WebRequest -Uri 'https://api.github.com/user' -UseBasicParsing -Headers @{
            Authorization = "token $plain"
            'User-Agent'  = 'wmux-release-token-rotation'
        }
    } catch {
        Write-Error "GitHub rejected the token: $($_.Exception.Message)"
        return
    }

    $login = ($response.Content | ConvertFrom-Json).login
    $scopes = @("$($response.Headers['X-OAuth-Scopes'])" -split ',\s*' | Where-Object { $_ })
    $expiry = "$($response.Headers['GitHub-Authentication-Token-Expiration'])"
    Write-Verbose "Token owner: $login; scopes: $($scopes -join ', '); expiry: $(if ($expiry) { $expiry } else { 'none' })."

    if (-not $scopes) {
        # Fine-grained tokens report no OAuth scopes, and winget-releaser
        # does not work with them (winget-releaser issue #172).
        Write-Error 'The token reports no scopes. Fine-grained tokens are not supported by winget-releaser; use a classic PAT with public_repo and workflow.'
        return
    }
    $missing = @()
    if (-not ($scopes -contains 'public_repo' -or $scopes -contains 'repo')) { $missing += 'public_repo' }
    if ($scopes -notcontains 'workflow') { $missing += 'workflow' }
    if ($missing) {
        Write-Error "The token is missing scope(s): $($missing -join ', '). It has: $($scopes -join ', ')."
        return
    }

    $extra = $scopes | Where-Object { $_ -notin 'public_repo', 'repo', 'workflow' }
    if ($scopes -contains 'repo') {
        Write-Warning "The token has full 'repo' scope (private repositories too); 'public_repo' is enough for wmux."
    }
    if ($extra) {
        Write-Warning "The token has scopes the release does not need: $($extra -join ', ')."
    }
    if (-not $expiry) {
        Write-Warning 'The token never expires. A leaked non-expiring token stays valid until you delete it.'
    }

    # --- store it ----------------------------------------------------------------
    $i = 0
    foreach ($name in $SecretName) {
        $i++
        Write-Progress -Activity $activity -Status "Setting $name" -PercentComplete (50 + 40 * $i / $SecretName.Count)
        if ($PSCmdlet.ShouldProcess("$Repository secret $name", "Set to $login's token")) {
            Write-Verbose "Setting secret $name on $Repository via stdin."
            $plain | & $gh secret set $name --repo $Repository
            if ($LASTEXITCODE -ne 0) {
                Write-Error "gh secret set $name failed with exit code $LASTEXITCODE."
                return
            }
        }
    }
} finally {
    # Drop the plaintext as soon as it is no longer needed.
    $plain = $null
    Remove-Variable plain, secure -ErrorAction SilentlyContinue -WhatIf:$false
}

# --- verify --------------------------------------------------------------------
Write-Progress -Activity $activity -Status 'Verifying' -PercentComplete 95
$listed = & $gh secret list --repo $Repository --json name,updatedAt | ConvertFrom-Json
Write-Progress -Activity $activity -Completed

if (-not $UseGhCliToken -and -not $WhatIfPreference) {
    Write-Host "Done. Delete the previous wmux-release token at https://github.com/settings/tokens (keep $tokenNote)."
}

foreach ($name in $SecretName) {
    $entry = $listed | Where-Object name -EQ $name
    [pscustomobject]@{
        Repository = $Repository
        Secret     = $name
        Owner      = $login
        Scopes     = $scopes -join ', '
        Expires    = if ($expiry) { $expiry } else { 'never' }
        UpdatedAt  = if ($entry) { [datetime]$entry.updatedAt } else { $null }
    }
}
