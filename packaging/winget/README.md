# Publishing blackbox to winget

Winget installs from a public URL, so every release needs the setup installer on the
GitHub Releases page. The release workflow builds it and also generates the manifests.

## First release (manual, once)

1. Tag and push a release (`git tag v0.3.0 && git push --tags`). The `Release` workflow builds
   `bb-v0.3.0-setup-x64.exe` and publishes it.
2. Download the `winget-manifests` artifact from that workflow run, or generate it yourself:

   ```powershell
   ./packaging/winget/generate.ps1 -Version 0.3.0 -Sha256 <sha256 of the setup exe> -OutDir ./winget-out
   winget validate --manifest ./winget-out/manifests/a/AnakinSkywalker0/blackbox/0.3.0
   ```

3. Test it locally before submitting. This installs from your own manifest:

   ```powershell
   winget settings --enable LocalManifestFiles
   winget install --manifest ./winget-out/manifests/a/AnakinSkywalker0/blackbox/0.3.0
   ```

4. Submit it to Microsoft. This opens a public pull request from your GitHub account:

   ```powershell
   winget install wingetcreate
   wingetcreate submit ./winget-out/manifests/a/AnakinSkywalker0/blackbox/0.3.0
   ```

   Microsoft's bots validate it (including a scan of the installer), then a reviewer approves.
   That usually takes a few days. Unsigned installers can get extra scrutiny.

After it is merged, anyone can run:

```
winget install AnakinSkywalker0.blackbox
winget upgrade AnakinSkywalker0.blackbox
```

## Later releases

Tag a new version and submit the new manifests the same way, or automate it with the
[winget-releaser](https://github.com/vedantmgoyal9/winget-releaser) action, which needs a
classic personal access token with `public_repo` stored as the repo secret `WINGET_TOKEN`.

## Notes

- Never change `AppId` in `installer/blackbox.iss`. Winget and Windows match upgrades on it
  (the manifest's `ProductCode` is that GUID plus `_is1`).
- `bb update` can replace the installed `bb.exe` with a newer version without winget knowing.
  That is harmless: a later `winget upgrade` just installs over it.
