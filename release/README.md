# Publishing WhisperDrop 0.2.0

1. Build the macOS DMG and Windows executable.
2. On a Windows release workstation, run `iscc installer\WhisperDrop.iss` to make the setup executable.
3. Code-sign the executable and installer with the organization's Authenticode certificate.
4. Sign and notarize the macOS app/DMG with the Apple Developer certificate and notarization profile.
5. Calculate SHA-256 for the signed artifacts and replace the placeholders in `latest.json`.
6. Host `latest.json` and both artifacts over HTTPS, then point the product's release channel at that URL.

Do not publish unsigned artifacts as production releases. Certificate files, Apple IDs, passwords and private update signing keys are intentionally not stored in this repository.
