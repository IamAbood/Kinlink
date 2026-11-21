# Publishes the current version as a GitHub Release so every installed copy can auto-update.
#   - builds native\ (version comes from native\Cargo.toml)
#   - copies dist\Kinlink.exe (+ a versioned copy)
#   - tags v<version> if needed and pushes the tag
#   - creates the release with Kinlink.exe and "sha256: <hash>" in the notes
#     (the in-app updater refuses downloads whose hash doesn't match)
#
# One-time setup:  winget install GitHub.cli   then   gh auth login
# Usage:           powershell -ExecutionPolicy Bypass -File scripts\release.ps1 [-Notes "what changed"]
param([string]$Notes = "")
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$repo = 'IamAbood/Kinlink'

$ver = (Select-String -Path "$root\native\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$tag = "v$ver"
Write-Host "Releasing $tag"

cargo build --release --manifest-path "$root\native\Cargo.toml"
if ($LASTEXITCODE -ne 0) { throw "build failed" }

New-Item -ItemType Directory -Force "$root\dist" | Out-Null
$exe = "$root\dist\Kinlink.exe"
Copy-Item "$root\native\target\release\pclone.exe" $exe -Force
Copy-Item $exe "$root\dist\Kinlink-$ver.exe" -Force
$sha = (Get-FileHash $exe -Algorithm SHA256).Hash.ToLower()

if (-not (git -C $root tag -l $tag)) {
    git -C $root tag -a $tag -m "Beta $ver"
}
git -C $root push origin $tag

$body = "Parsec Clone $ver`n`n$Notes`n`nsha256: $sha"
gh release view $tag --repo $repo *> $null
if ($LASTEXITCODE -eq 0) {
    # Release exists (e.g. re-run): replace the exe and notes.
    gh release upload $tag $exe --repo $repo --clobber
    gh release edit $tag --repo $repo --notes $body
} else {
    gh release create $tag $exe --repo $repo --title $tag --prerelease --notes $body
}
if ($LASTEXITCODE -ne 0) { throw "gh release failed (run 'gh auth login' once?)" }
Write-Host "Done: https://github.com/$repo/releases/tag/$tag  (sha256 $sha)"
