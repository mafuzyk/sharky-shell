# Releasing sharky 2.0

The Rust port lives in `rust/` on the `rust` branch. `main` is an ancestor of
it, so the merge is a fast-forward. Everything below up to the push is local.

## 1. Final checks

```sh
cd rust
systemd-run --user --scope --quiet -p MemoryMax=2G -- cargo test -j 6
systemd-run --user --scope --quiet -p MemoryMax=2G -- cargo clippy -j 6 --all-targets
systemd-run --user --scope --quiet -p MemoryMax=2G -- cargo build --release --locked -j 6
./target/release/sharky --version
```

The differential suite needs a local Ruby v1 checkout; it is skipped when
`.ruby-oracle/bin/sharky` is absent (override with `SHARKY_REF`).

## 2. Merge and tag

```sh
cd ..                       # repository root
git checkout main
git merge --ff-only rust
git tag -a v2.0.0 -m "sharky 2.0.0: Rust implementation"
git push origin main
git push origin v2.0.0
```

Pushing is the first irreversible step: it changes the default branch to the
Rust implementation.

## 3. Build the packages

`sharky-shell-git` builds from the repository:

```sh
cd rust/packaging
makepkg -f --nodeps
makepkg --printsrcinfo > .SRCINFO
```

`sharky-shell-bin` installs the tarball attached to the GitHub release. Bump
`pkgver` in `rust/packaging/bin/PKGBUILD` first, then refresh both checksums
(the release tarball and the tag's `LICENSE`):

```sh
cd rust/packaging/bin
updpkgsums                  # or sha256sum the two source files by hand
makepkg -f --nodeps
makepkg --printsrcinfo > .SRCINFO
```

Both packages should contain:

```
usr/bin/sharky
usr/share/man/man1/sharky.1.gz
usr/share/sharky/config.sharky
usr/share/sharky/theme.sharky
usr/share/licenses/<pkgname>/LICENSE
```

## 4. AUR

There are two independent AUR repositories:

- `ssh://aur@aur.archlinux.org/sharky-shell-git.git`
- `ssh://aur@aur.archlinux.org/sharky-shell-bin.git` (x86_64 only)

```sh
git clone ssh://aur@aur.archlinux.org/sharky-shell-git.git
cp rust/packaging/PKGBUILD rust/packaging/.SRCINFO sharky-shell-git/
cd sharky-shell-git
git add PKGBUILD .SRCINFO
git commit -m "upgpkg: sharky-shell-git <version>"
git push
```

```sh
git clone ssh://aur@aur.archlinux.org/sharky-shell-bin.git
cp rust/packaging/bin/PKGBUILD rust/packaging/bin/.SRCINFO sharky-shell-bin/
cd sharky-shell-bin
git add PKGBUILD .SRCINFO
git commit -m "upgpkg: sharky-shell-bin <version>"
git push
```

The first upload of a package uses an `initial upload:` message; later updates
use `upgpkg:`. The `-bin` package must be updated only after the GitHub release
for that tag exists, because its sources point at the release assets.

## 5. After the push

```sh
git clone https://aur.archlinux.org/sharky-shell-git.git /tmp/aur-git
cd /tmp/aur-git && makepkg -si
sharky --version

git clone https://aur.archlinux.org/sharky-shell-bin.git /tmp/aur-bin
cd /tmp/aur-bin && makepkg -si
sharky --version
```

If a package fails on a clean machine, check `namcap PKGBUILD` and
`namcap *.pkg.tar.zst`.
