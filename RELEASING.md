# Releasing loudeq

Notes to self. Covers a GitHub release (the zip people download directly) and a
Microsoft Store submission. They are independent — you can do either alone —
but a Store submission should normally have a matching GitHub tag so the source
that produced the package is identifiable.

Not linked from the README on purpose; this is a maintainer document.

## ⚠️ The three things that have actually broken releases

1. **Build with the GNU toolchain, never MSVC.** MSVC-linked exes depend on
   `VCRUNTIME140.dll` (the VC++ Redistributable) and crash on machines without
   it — and they fail Store certification 10.2.4.1 for an undeclared
   dependency. This shipped broken twice: the 0.2.0 Store package went *live*
   with a cert warning, and the v0.7.1 zip crashed on launch.
2. **Run `makeappx` from PowerShell, not Git Bash.** Git Bash rewrites `/d` into
   a path like `D:/` and the command fails with nothing useful in the error.
3. **Don't forget the GitHub release.** It has been missed before — the Store
   went out and the tag never did.

## Version numbers — read this before bumping anything

Three version numbers exist and **they are not the same**:

| Where | Example | Bump when |
|---|---|---|
| Git tag / GitHub release | `v0.9.1` | Every release. This is *the* app version. |
| `store/AppxManifest.xml` `Identity/@Version` | `0.2.3.0` | Every Store submission. Must be 4 parts and strictly greater than the last accepted one. |
| `Cargo.toml` `version` | `0.1.0` | Never has been. It is stale and nothing reads it. |

The tag line and the Store line advance independently and will never agree —
that is expected, not a bug to "fix". If you ever want to reconcile them, do it
deliberately as its own change, because the Store version can only ever go up.

## Before you start

```powershell
# GNU must be the active toolchain for this repo
rustup show                 # look for stable-x86_64-pc-windows-gnu (default)
rustup override set stable-x86_64-pc-windows-gnu   # if it isn't

cargo test
cargo build --release
```

Verify the binaries are clean — this is the check that would have caught both
broken releases:

```powershell
foreach ($exe in "target\release\loudeq.exe","target\release\loudeq-tray.exe") {
    $s = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($exe))
    if ($s -match 'VCRUNTIME[0-9]*\.dll') { "$exe : MSVC BUILD - DO NOT SHIP ($($Matches[0]))" }
    elseif ($s -match 'kernel32\.dll')    { "$exe : clean" }
    else                                  { "$exe : scan failed - check manually" }
}
```

Both exes must say `clean`. The `kernel32` branch is a deliberate positive
control: a check that can only ever report "clean" is worse than no check, so
if it can't find a string that is definitely present, it says so instead.
(`Select-String -Pattern 'VCRUNTIME' <exe>` also works and does read binaries —
verified — but it gives no such assurance on a null result.)

Smoke-test the tray by hand: launch `target\release\loudeq-tray.exe`, toggle
loudness, confirm the icon changes and a balloon appears with the app named
**LoudEQ** (not a `NotifyIconGeneratedAumid_…` string).

## A. GitHub release

```powershell
$v = "v0.9.2"     # <- set this

# Stage exactly the two exes, nothing else
$stage = "$env:TEMP\loudeq-$v"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null
Copy-Item target\release\loudeq.exe,target\release\loudeq-tray.exe $stage
Compress-Archive "$stage\*" "$env:TEMP\loudeq-$v-x86_64-windows.zip" -Force

git tag $v
git push origin $v
gh release create $v "$env:TEMP\loudeq-$v-x86_64-windows.zip" `
  --title "loudeq $v" --notes "..."
```

Asset naming has been consistent: `loudeq-<tag>-x86_64-windows.zip`. Keep it.

Release notes: describe what a *user* notices. Registry-level detail belongs in
commit messages.

## B. Microsoft Store submission

### 1. Bump the package version

Edit `store/AppxManifest.xml`:

```xml
<Identity Name="ardenden.LoudEQ"
          Publisher="CN=37B6B9F3-38C7-4DC2-9CCE-F08F6BB3B8C6"
          Version="0.2.4.0"          <!-- was 0.2.3.0 -->
          ProcessorArchitecture="x64" />
```

`DisplayName` must stay exactly `LoudEQ` — it has to match the reserved name in
Partner Center or certification rejects the package.

### 2. Build the MSIX

Pack a **clean staging directory**. Do not point `makeappx` at `store/` itself:
it would sweep in the previously built `.msix` files sitting there.

```powershell
$ver   = "0.2.4"
$stage = "$env:TEMP\loudeq-msix"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null

Copy-Item store\AppxManifest.xml $stage
Copy-Item store\Assets $stage -Recurse
Copy-Item target\release\loudeq.exe,target\release\loudeq-tray.exe $stage

$makeappx = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\makeappx.exe"
& $makeappx pack /d $stage /p "store\loudeq-$ver.msix" /o
```

`store/*.msix` is gitignored — the packages are build output, not source.

### 3. Submit in Partner Center

Upload the `.msix`, then check the listing against `store/listing.md`, which is
the source of truth for the copy. If the description or search terms changed,
update that file in git too so the repo matches what is live.

**Search terms — the rule that got a submission rejected:** max 7, ≤30 chars
each, and **no product titles you don't publish**. "windows 11", "windows
surround sound", "windows audio enhancements" were all rejected even though
Partner Center happily accepted them in the form. Generic terms only. Prose in
the *description* is not subject to this — only the hidden search terms.

Certification has been taking hours rather than the advertised 1–3 days.

### 4. After it goes live

Install from the Store on a clean-ish machine if you can, launch the tray, and
toggle once. The 0.2.0 VCRUNTIME breakage was only found this way.

## C. Downstream

Anything that consumes this crate by git tag needs its pin bumped separately
after the tag is pushed; that lives with the consumer, not here.

## Checklist

- [ ] `rustup show` says GNU
- [ ] `cargo test` passes
- [ ] `cargo build --release`
- [ ] No `VCRUNTIME` string in either exe
- [ ] Tray smoke test — icon toggles, balloon says "LoudEQ"
- [ ] `AppxManifest.xml` version bumped (Store only)
- [ ] MSIX packed from a clean staging dir, in PowerShell
- [ ] Git tag pushed
- [ ] GitHub release created with the zip attached
- [ ] `store/listing.md` matches what is live in Partner Center
- [ ] downstream tag pins bumped, if they should follow
