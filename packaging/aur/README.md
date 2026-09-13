# AUR packaging

`PKGBUILD` builds the tagged release from GitHub. `.SRCINFO` is generated,
never hand-edited — regenerate it whenever `PKGBUILD` changes:

```sh
makepkg --printsrcinfo > .SRCINFO
```

To publish, these two files are the whole content of the AUR repository:

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
