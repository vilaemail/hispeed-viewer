param([Parameter(Mandatory)][string]$Version)
$content = Get-Content 'Cargo.toml'
$content = $content -replace 'version = "[^"]*"', "version = `"$Version`""
Set-Content 'Cargo.toml' $content
