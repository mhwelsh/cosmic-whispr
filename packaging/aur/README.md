# AUR packaging

`PKGBUILD` builds the tagged release from GitHub. `.SRCINFO` is generated,
never hand-edited — regenerate it whenever `PKGBUILD` changes:

```sh
makepkg --printsrcinfo > .SRCINFO
```

Users do not need the AUR for this: `makepkg -si` from this directory works
on any Arch-derived system, and builds with their own CPU tuning rather than
whatever the packager's machine had.

To publish when AUR registration is open again, these two files are the whole
content of the AUR repository:

```sh
git clone ssh://aur@aur.archlinux.org/cosmic-whispr.git aur-cosmic-whispr
cp PKGBUILD .SRCINFO aur-cosmic-whispr/
cd aur-cosmic-whispr && git add PKGBUILD .SRCINFO
git commit -m 'Initial import: cosmic-whispr 0.1.0' && git push
```

That needs an AUR account with an SSH key registered against it.

## Releasing a new version

Bump `pkgver`, reset `pkgrel=1`, then `updpkgsums` to pick up the new tarball
checksum and `makepkg --printsrcinfo > .SRCINFO` again.
