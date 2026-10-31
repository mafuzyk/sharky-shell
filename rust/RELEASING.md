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

The differential suite needs the frozen Ruby tree; it is skipped when
`../bin/sharky` is absent.

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

## 3. Build the package

```sh
cd rust/packaging
makepkg -f --nodeps         # builds sharky-shell-git
makepkg --printsrcinfo > .SRCINFO
```

Package contents should be:

```
usr/bin/sharky
usr/share/man/man1/sharky.1.gz
usr/share/sharky/config.sharky
usr/share/sharky/theme.sharky
usr/share/licenses/sharky-shell-git/LICENSE
```

## 4. AUR

The AUR repository is `ssh://aur@aur.archlinux.org/sharky-shell-git.git`.

```sh
git clone ssh://aur@aur.archlinux.org/sharky-shell-git.git
# first release only; afterwards clone the existing repository
cp PKGBUILD .SRCINFO /path/to/sharky-shell-git/
cd /path/to/sharky-shell-git
git add PKGBUILD .SRCINFO
git commit -m "sharky-shell-git: initial upload"
git push
```

For a stable `sharky-shell` package as well, point `source` at the GitHub
tarball for the tag and let `updpkgsums` fill `sha256sums`:

```
source=("$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('<filled by updpkgsums>')
```

## 5. After the push

```sh
git clone https://aur.archlinux.org/sharky-shell-git.git /tmp/aur-check
cd /tmp/aur-check && makepkg -si
sharky --version
```

If the package fails on a clean machine, check `namcap PKGBUILD` and
`namcap *.pkg.tar.zst`.
