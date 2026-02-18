# Signing Setup

## Generate key

Run in powershell:
```
$params = @{
    Type = 'Custom'
    Subject = 'CN=Name Surname'
    TextExtension = @(
        '2.5.29.37={text}1.3.6.1.5.5.7.3.3',
        '2.5.29.17={text}upn=email@address.com' )
    KeyUsage = 'DigitalSignature'
    KeyAlgorithm = 'RSA'
    KeyLength = 2048
    CertStoreLocation = 'Cert:\CurrentUser\My'
    NotAfter = (Get-Date).AddYears(100)
}
New-SelfSignedCertificate @params
```

## Export key

Run `certmgr.msc` navigate to Personal->Certificates and find certificate with `Name Surname` you set in [Generate key](#generate-key). Right click, all tasks, export, export private key and export in pfx format. Save into this folder with name `release.pfx`.
