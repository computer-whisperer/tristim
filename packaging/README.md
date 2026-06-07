# Packaging

`aur/PKGBUILD` is the staging copy of the Arch package that installs both
applications (`tristim` CLI + `tristim-gui`), the udev rule, and the desktop
entry. The AUR proper is a separate git remote
(`ssh://aur@aur.archlinux.org/tristim.git`); this directory is where the
PKGBUILD evolves alongside the code it packages.

## Local test build

The PKGBUILD sources a GitHub release tarball, but makepkg uses a local file
of the same name when present — so any commit can be test-built without a
tag:

```sh
cd $(mktemp -d)
cp ~/workspace/tristim/packaging/aur/PKGBUILD .
git -C ~/workspace/tristim archive --prefix=tristim-0.2.2/ \
  -o "$PWD/tristim-0.2.2.tar.gz" HEAD
makepkg -f          # build + test + package
pacman -Qlp tristim-*.pkg.tar.zst
```

## Release checklist

1. Tag the release: `git tag v0.2.2 && git push origin v0.2.2`.
2. In a clone of the AUR repo, copy `aur/PKGBUILD` in, then pin the real
   tarball hash: `updpkgsums` (replaces the `SKIP` placeholder).
3. `makepkg -f` once against the published tarball (catches a tag/lockfile
   mismatch), then `makepkg --printsrcinfo > .SRCINFO`.
4. Commit PKGBUILD + .SRCINFO to the AUR repo and push.

On version bumps: update `pkgver`, reset `pkgrel=1`, repeat from step 2.
