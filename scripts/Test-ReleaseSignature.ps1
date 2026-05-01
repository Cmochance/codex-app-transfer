param(
    [Parameter(Mandatory = $true)][string]$File,
    [string]$Signature,
    [string]$PublicKey = "release/Codex-App-Transfer-release-public.pem"
)

$ErrorActionPreference = "Stop"

if (-not $Signature) {
    $Signature = "$File.sig"
}

$pem = Get-Content -LiteralPath $PublicKey -Raw -Encoding ascii
$rsa = [System.Security.Cryptography.RSA]::Create()
$rsa.ImportFromPem($pem)

$bytes = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $File).Path)
$sigBytes = [Convert]::FromBase64String((Get-Content -LiteralPath $Signature -Raw -Encoding ascii).Trim())
$ok = $rsa.VerifyData(
    $bytes,
    $sigBytes,
    [System.Security.Cryptography.HashAlgorithmName]::SHA256,
    [System.Security.Cryptography.RSASignaturePadding]::Pkcs1
)

if ($ok) {
    Write-Host "SIGNATURE_OK $File"
    exit 0
}

Write-Error "SIGNATURE_INVALID $File"
exit 1
