# Code signing for blackbox

Windows shows a blue "unknown publisher" SmartScreen screen for unsigned installers, and
unsigned programs that install themselves are more likely to be flagged by antivirus. Signing
fixes both, but it needs a certificate that only the project owner can obtain. Nothing in this
repository can do that for you. This page says what to apply for and where signing plugs in.

## Options

| Option | Cost | Who can get it | Notes |
|---|---|---|---|
| **SignPath Foundation** | Free | Open-source projects (this one is MIT) | Apply at signpath.org/foundation. They check the project is genuine open source, then sign builds made by your GitHub Actions workflow. Approval takes days to weeks. The certificate is in the foundation's name. Best fit. |
| **Azure Trusted Signing** | About $10 a month | Individuals or organisations that pass identity validation (availability varies by country) | Signs with a Microsoft-managed certificate. Works well with GitHub Actions through `azure/trusted-signing-action`. |
| **Standard (OV) certificate** | About $100 to $400 a year | Anyone who can be validated | Since 2023 the private key must live on a hardware token or cloud HSM, which makes CI harder. SmartScreen reputation still builds slowly. |
| **EV certificate** | Higher | Registered organisations | Gives immediate SmartScreen trust, but mostly needs a hardware token. Overkill here. |

A new signing certificate, even a valid one, still starts with little SmartScreen reputation.
The warning fades as more people install the signed file.

## What to sign

1. `bb.exe`, the program itself (inside the zip and inside the installer).
2. `bb-v*-setup-x64.exe`, the installer.

Sign `bb.exe` first, then build the installer around the signed copy, then sign the installer.
The build order in `.github/workflows/release.yml` is already: build, package the zip, build
the installer. Signing slots in after "Build" (for `bb.exe`) and after "Build the Windows
installer" (for the setup exe).

## Applying to SignPath Foundation (checklist)

- The repository is public and MIT licensed. Both are already true.
- A project homepage and a short description. The README works.
- A statement of what the program does, and that it has no network use except `bb update`.
  The README states this.
- The release workflow must build from source in GitHub Actions, not on a laptop. It does.
- A code of conduct and a privacy statement may be asked for. The "Where your data lives"
  section of the README covers privacy.

Once approved they give you an organisation id, project slug and a CI token. Store the token as
the repository secret `SIGNPATH_API_TOKEN`, then add their `signpath/github-action-submit-signing-request`
step after the build. I can add and test that step once you have the credentials.

## After signing

- The installer's publisher shows as the signer instead of "Unknown publisher".
- The winget manifest can stay as it is. Signed installers pass Microsoft's validation with
  less manual review.
- Update the README line about the unsigned installer and the SmartScreen warning.
