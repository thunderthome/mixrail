# Packaging and releases

| File | What it is |
|---|---|
| `mixrail.service` | systemd user service for the engine (`/usr/bin/mixrail run`) |
| `mixrail.desktop` | app menu entry |
| `mixrail-tray.desktop` | tray icon, installed to `/etc/xdg/autostart` by the packages |
| `mixrail-engine.desktop` | engine autostart for systems without systemd |
| `install.sh` | install from source for one user, into `~/.local` (no root) |
| `aur/PKGBUILD`, `aur/.SRCINFO` | Arch User Repository package |
| `rpm/mixrail.spec` | Fedora package (also for COPR) |
| `Cargo.toml` → `[package.metadata.deb]` | Debian/Ubuntu package, built with `cargo deb` |
| `build-packages.sh` | builds the `.deb` and `.rpm` in Ubuntu/Fedora containers and test-installs them in fresh ones (needs Docker) |
| `../.github/workflows/release.yml` | builds the `.deb`, `.rpm` and Arch package on GitHub for every version tag and publishes them as a release |

All packages install the same files: `/usr/bin/mixrail`, the user service, the menu entry,
the tray autostart entry, README and license. The engine starts the first time the mixer
window is opened (it enables the user service, so it then starts at every login).

## Making a release

1. Set the new version in `Cargo.toml`, `aur/PKGBUILD` (`pkgver`, `pkgrel=1`) and
   `rpm/mixrail.spec` (`Version`, plus a `%changelog` entry), then run `cargo build` so
   `Cargo.lock` follows.
2. Optional: `packaging/build-packages.sh` to try the packages locally.
3. Commit, then tag and push:
   ```sh
   git tag v0.2.0
   git push && git push --tags
   ```
   GitHub builds the `.deb` and `.rpm` and attaches them to a new release.
4. **AUR** (once the release exists):
   ```sh
   cd packaging/aur
   updpkgsums                          # fills in the checksum of the release download
   makepkg --printsrcinfo > .SRCINFO
   makepkg -f                          # optional: check it builds
   ```
   Commit those changes here, then copy `PKGBUILD` and `.SRCINFO` into your clone of
   `ssh://aur@aur.archlinux.org/mixrail.git` and push.
5. **Fedora COPR** (optional, gives Fedora users `dnf install`): the COPR project builds
   from this repository with the spec at `packaging/rpm/mixrail.spec`; trigger a new build.

## One-time setup

- **AUR**: create an account at <https://aur.archlinux.org>, add your SSH public key in
  "My Account", then `git clone ssh://aur@aur.archlinux.org/mixrail.git` (an empty clone
  creates the package on first push).
- **COPR**: log in at <https://copr.fedorainfracloud.org> with a Fedora account, create a
  project "mixrail" for the current Fedora releases, enable "internet access during
  builds" (Cargo downloads the Rust libraries), and add a package with source type "SCM":
  this repository's URL and spec file `packaging/rpm/mixrail.spec`.

## Security updates

You don't need to watch anything by hand. Once the repository is on GitHub, turn these on
in **Settings → Advanced Security** (called "Code security" on some accounts):

- **Dependabot alerts** and **Dependabot security updates**: GitHub emails you when a
  library Mixrail uses has a known security problem, and opens a pull request with the fix.
- **Private vulnerability reporting**: lets people report problems privately (see
  `SECURITY.md`).

Also running automatically:

- `.github/workflows/audit.yml` checks all libraries every Monday; if one has a known
  security problem the run fails and GitHub emails you.
- `.github/dependabot.yml` proposes library updates weekly (one grouped pull request);
  the CI workflow tests each one.

When an alert comes in:

1. Merge Dependabot's pull request once its CI check is green (or run `cargo update` and
   commit `Cargo.lock` yourself).
2. Make a new release with a higher version number (steps above), so packages and the
   AUR pick up the fix.

Problems in PipeWire, ALSA or the C library are fixed by the distributions themselves,
not by a Mixrail release, because Mixrail uses the system's copies of those.

## Flatpak

Not provided on purpose: Mixrail creates system-wide audio devices, runs an engine at
login, talks to MIDI hardware and starts `pw-record`. The Flatpak sandbox would need to
be opened up for nearly all of that, and starting at login works differently there. The
native packages above fit an app like this much better.
