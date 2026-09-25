# git-cow

`git worktree add` that copies nothing. New worktrees are **copy-on-write clones** of an
existing one (APFS `clonefile`, btrfs/XFS/ZFS reflinks), including gitignored build
caches like `node_modules/`, `_build/`, `deps/` and `target/`. A new worktree costs ~0
disk and is ready to build immediately.

Made for parallel coding agents that each get their own worktree.

```console
$ git worktree add ../agent-1
Preparing worktree (new branch 'agent-1')
git-cow: apfs: cloned from /src/app, reused stat data for 20002 files, wrote 0 files; carried _build, deps, node_modules
HEAD is now at 1c2212f Add login page
```

## How it works

git does all the git work. Only `git worktree add` changes, via a small `git` wrapper:

1. `git worktree add --no-checkout <your arguments>`: git creates the branch and worktree
   metadata, validates everything, and handles every option (`-b`, `-B`, `--detach`,
   `--lock`, `--track`, ...).
2. `git cow populate`: clones the current worktree into the new one. On macOS a whole
   directory tree is one `clonefile(2)` call. Tracked files and ignored build caches are
   cloned; untracked scratch files are not.
3. The index is built for the new HEAD. For files the source index knows are clean, stat
   data is reused, so nothing is read. The rest is hashed, and only files that differ
   from HEAD are written by libgit2.
4. The `post-checkout` hook runs, like after a normal `git worktree add`.

If anything fails, the worktree falls back to git's own checkout. Every other git command
goes straight to the real git (the wrapper adds ~2–5 ms per call).

## Install

### devenv

```yaml
# devenv.yaml
inputs:
  git-cow:
    url: github:onnimonni/git-cow
    flake: false
imports:
  - git-cow/devenv-module
```

That's it: `git` in the devenv shell is now the wrapper. Options:

```nix
# devenv.nix
{ pkgs, ... }: {
  git-cow.enable = true; # default
  git-cow.git = pkgs.git; # the real git the wrapper runs
}
```

### Nix flake

```nix
inputs.git-cow.url = "github:onnimonni/git-cow";
# packages.${system}.default has bin/git (wrapper) and bin/git-cow
```

Put it before git in `PATH`, e.g. `nix profile install github:onnimonni/git-cow`.

### From source

```sh
cargo install --git https://github.com/onnimonni/git-cow
# the wrapper finds the real git as the next `git` in PATH
curl -o ~/.local/bin/git https://raw.githubusercontent.com/onnimonni/git-cow/main/wrapper/git
chmod +x ~/.local/bin/git # ~/.local/bin must come before the real git in PATH
```

## What gets carried into the new worktree

| | |
|---|---|
| tracked files | always (then fixed up to the new HEAD) |
| untracked, not ignored | never: agents start clean |
| gitignored | yes, except the ones below |
| never | sockets/fifos, `.hg` `.jj` `.svn` ..., `.worktrees` `.conductor` `.entire`, nested worktrees or repos (e.g. `.claude/worktrees/`), virtualenvs (`pyvenv.cfg`), CMake build dirs (`CMakeCache.txt`) |
| not by default | `.venv/`, `venv/`, `.devenv/`, `.direnv/`, `tmp/`, `log/`, `*.pid`, `*.sock` |

The "never" entries contain absolute paths to their own location or live state: a clone
would keep building into (or running from) the source worktree.

### `.worktreeinclude`

Same file as [worktrunk](https://github.com/max-sixty/worktrunk) and Claude Code desktop
use: gitignore syntax, commit it to the repo. When it exists, only ignored paths matching
it are carried (this also overrides the "not by default" list):

```gitignore
# .worktreeinclude
node_modules/
_build/
deps/
.env
```

### Configuration

```sh
git config --add cow.exclude '.cache/'        # never carry (gitignore syntax, repeatable)
git config --add cow.exclude '!tmp/'          # carry a "not by default" entry anyway
git config cow.requireInclude true            # carry nothing without .worktreeinclude (like Claude Code)
git config cow.carryIgnored false             # only tracked files
GIT_COW_DISABLE=1 git worktree add ...        # plain git
```

## Languages

`bench/languages.sh` creates a small project per language, then compares a plain
`git worktree add` + setup + build against the wrapper + the same build. It also checks
correctness: the program is edited and rebuilt in the new worktree, it must print the new
output, and no file in the source worktree may change.

LANGUAGE_TABLE

Tiny projects mostly measure tool startup and git-cow's up-to-1-second safety wait (see
below). The gains grow with dependency trees and compile times. Measured on real-sized
projects:

| project | plain worktree + setup | git-cow |
|---|---|---|
| Elixir (phoenix, ecto, jason) | 14.7 s (`mix deps.get` + `mix compile`) | 0.06 s + 0.4 s `mix compile` (1 file) |
| npm (next, react, typescript, eslint; 348 MB) | 4.8 s, 495 MB (`npm ci`) | 1.2 s, 7 MB |
| 50 MB git-lfs file + 20k files | 3.9 s | 1.3 s |

Notes:

- **bun** already clones packages from its global cache on macOS: no gain.
- **Go** keeps its build cache in `~/Library/Caches/go-build` (shared): little to gain.
- **Python**: virtualenvs hold absolute paths and are never carried; `uv sync` is fast.
- **CMake**: build dirs hold absolute paths and are never carried; configure again.

## Correctness

- git handles every branch and worktree rule. If `git cow populate` fails, the worktree
  is reset and filled by git's own checkout.
- Stat data is only reused for entries the source index proves clean: the entry isn't
  racy, the source file matched it before the clone started, and it didn't change during
  the clone. It's also skipped when `.gitattributes` differ between the source and the
  new HEAD. Everything else is hashed.
- Clones keep their old mtime, so git relies on ctime to spot later edits. git compares
  ctime in whole seconds, so `git cow populate` returns only after the second of the last
  clone has passed. Otherwise an edit that restores the mtime (`cp -p`, `rsync -t`)
  within that second would go unnoticed.
- git-lfs works without the git-lfs binary. Smudged files are verified against the
  pointer's sha256. Changed files are cloned from `.git/lfs/objects` after the same check.
  Missing or damaged objects stay pointers and are reported (`git lfs pull`). Pointers
  with `ext-*` extensions are left alone.
- Submodules are left uninitialised, like a fresh `git worktree add`.

`tests/e2e.sh` runs the real git and git-lfs against wrapper-made worktrees: status, diff,
fsck, commits, rebase, merge, stash, cherry-pick, submodules, move, remove, lock, and
hooks.

## Filesystems

| | |
|---|---|
| macOS | APFS: `clonefile(2)`, whole trees at once |
| Linux | btrfs, XFS (`reflink=1`), bcachefs, ZFS 2.2+, OCFS2: `FICLONE` per file |
| others / other volume | regular checkout (still correct, no savings) |

## Development

```sh
devenv shell
cargo test              # unit/integration tests (block sharing checked via F_LOG2PHYS / FIEMAP)
tests/e2e.sh            # real git + git-lfs through the wrapper
bench/languages.sh rust # language matrix (needs nix and network)
devenv test             # all of the above except the benchmark
```

## Prior art

- [git-wtclone](https://github.com/spofdamon/git-wtclone): the same idea for macOS, in
  Python; the stat-reuse trick comes from there
- [worktrunk](https://github.com/max-sixty/worktrunk): worktree manager for agents,
  `.worktreeinclude`
- [gh-wt](https://github.com/HikaruEgashira/gh-wt): CoW worktrees via OverlayFS/APFS

## License

MIT
