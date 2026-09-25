#!/usr/bin/env bash
# End-to-end check that worktrees made through the git wrapper behave like normal ones
# with the real git / git-lfs CLIs. Usage: tests/e2e.sh [path/to/git-cow dir]
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin_dir=${1:-$root/target/release}
# the wrapper shadows the real git; git-cow next to it
export PATH="$root/wrapper:$bin_dir:$PATH"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

export GIT_CONFIG_GLOBAL=$tmp/gitconfig GIT_CONFIG_NOSYSTEM=1
git config --global user.name test
git config --global user.email test@example.com
git config --global commit.gpgsign false
git config --global init.defaultBranch main
git config --global protocol.file.allow always

failures=0
check() { # check <description> <command...>
  local desc=$1; shift
  if out=$("$@" 2>&1); then echo "ok   $desc"; else echo "FAIL $desc"; sed 's/^/     /' <<<"$out"; failures=$((failures + 1)); fi
}
fails() { ! "$@"; }
clean() { local s; s=$(git -C "$1" status --porcelain) && [[ -z $s ]]; }
same() { [[ $("${@:2}") == "$1" ]]; }
export -f clean same fails
# ignored build caches are only carried where files can be cloned (not e.g. on ext4)
echo probe > "$tmp/reflink-probe"
if [[ $(uname) == Darwin ]] || cp --reflink=always "$tmp/reflink-probe" "$tmp/reflink-probe2" 2>/dev/null; then
  carried() { test -f "$1/node_modules/dep/index.js"; }
else
  carried() { test ! -e "$1/node_modules"; }
fi
export -f carried

# --- fixture: repo with lfs, a submodule, some history ---
git init -q sub && git -C sub commit -q --allow-empty -m sub
git init -q repo && cd repo
git lfs install --local >/dev/null && git lfs track '*.bin' >/dev/null
head -c 5000000 /dev/urandom > model.bin
mkdir src && for i in $(seq 1 200); do echo "line $i" > "src/f$i.txt"; done
echo "hello" > README
printf "node_modules/\n.venv/\n" > .gitignore
git submodule add -q ../sub sub 2>/dev/null
git add . && git commit -qm c1
mkdir -p node_modules/dep .venv && echo "module.exports = 1" > node_modules/dep/index.js && echo "home = /usr/bin" > .venv/pyvenv.cfg
echo "v2" >> README && git commit -qam c2
main=$PWD

check "worktree add new branch" git worktree add -q -b feat ../wt
check "worktree add detached HEAD~1" git worktree add -q --detach ../old HEAD~1
wt=$tmp/wt old=$tmp/old

echo "--- read-only commands"
check "status clean" clean "$wt"
check "status clean (detached)" clean "$old"
check "diff empty" git -C "$wt" diff --exit-code
check "diff --cached empty" git -C "$wt" diff --cached --exit-code
check "branch is feat" same feat git -C "$wt" branch --show-current
check "worktree list shows 3" same 3 bash -c "git worktree list | wc -l | tr -d ' '"
check "fsck" git -C "$wt" fsck --no-progress
check "ignored node_modules carried (if the fs can clone)" carried "$wt"
check "virtualenv not carried" test ! -e "$wt/.venv"
check "update-index --refresh" git -C "$wt" update-index --refresh
check "ls-files matches HEAD tree" same "$(git ls-tree -r --name-only HEAD)" git -C "$wt" ls-files
check "lfs file smudged" same "$(sha256sum <model.bin | cut -d' ' -f1)" bash -c "sha256sum <'$wt/model.bin' | cut -d' ' -f1"
check "lfs status clean" bash -c "out=\$(git -C '$wt' lfs status --porcelain) && [[ -z \$out ]]"
check "lfs fsck" git -C "$wt" lfs fsck --pointers

echo "--- change detection on cloned files"
printf 'line X\n' > "$wt/src/f1.txt"                       # same size as "line 1"
touch -r "$main/src/f1.txt" "$wt/src/f1.txt"               # and same mtime
check "same-size same-mtime edit detected" same " M src/f1.txt" git -C "$wt" status --porcelain
git -C "$wt" checkout -q -- src/f1.txt
check "checkout -- restores" clean "$wt"
echo "source edit" >> "$main/src/f2.txt"
check "edit in source doesn't leak (CoW)" same "line 2" cat "$wt/src/f2.txt"
check "worktree still clean" clean "$wt"
git -C "$main" checkout -q -- src/f2.txt

echo "--- commits"
echo "wt change" >> "$wt/README"
check "commit -a" git -C "$wt" commit -qam "wt commit"
check "branch updated in main repo" same "wt commit" git -C "$main" log -1 --format=%s feat
echo new > "$wt/new.txt"
check "add + commit new file" bash -c "git -C '$wt' add new.txt && git -C '$wt' commit -qm new"
head -c 1000000 /dev/urandom > "$wt/model.bin"
check "commit lfs change" git -C "$wt" commit -qam "lfs change"
check "committed as lfs pointer" bash -c "git -C '$wt' show HEAD:model.bin | grep -q git-lfs.github.com"
check "status clean after commits" clean "$wt"
check "commit in detached worktree" bash -c "echo x > '$old/detached.txt' && git -C '$old' add detached.txt && git -C '$old' commit -qm detached"
check "branch from detached" git -C "$old" switch -q -c from-detached

echo "--- history editing"
check "stash / pop" bash -c "echo s >> '$wt/README' && git -C '$wt' stash -q && git -C '$wt' stash pop -q && git -C '$wt' checkout -q -- README"
check "rebase onto main" bash -c "cd '$main' && echo m > m.txt && git add m.txt && git commit -qm main-only && git -C '$wt' rebase -q main"
check "merge" bash -c "git -C '$old' merge -q --no-edit feat"
check "reset --hard HEAD~1" git -C "$wt" reset -q --hard HEAD~1
check "cherry-pick" bash -c "git -C '$wt' cherry-pick feat@{1} >/dev/null"
check "switch to other branch" bash -c "git -C '$wt' switch -q -c tmp-branch && git -C '$wt' switch -q feat"
check "clean -fdx" bash -c "touch '$wt/junk' && git -C '$wt' clean -qfdx && [[ ! -e '$wt/junk' ]]"
check "status clean after history editing" clean "$wt"

echo "--- submodules"
check "submodule dir empty (not a broken clone)" same "" ls -A "$wt/sub"
check "submodule update --init" git -C "$wt" submodule update -q --init
check "submodule status" bash -c "git -C '$wt' submodule status | grep -qv '^-'"

echo "--- wrapper: git worktree add options"
check "git resolves to the wrapper" same "$root/wrapper/git" type -P git
check "other commands pass through" same "$(command "$(type -ap git | sed -n 2p)" --version)" git --version
printf '#!/bin/sh\necho "$@" > "%s/hook-ran"\n' "$tmp" > "$main/.git/hooks/post-checkout"
chmod +x "$main/.git/hooks/post-checkout"
check "prints HEAD like git" bash -c "git worktree add ../w-out 2>/dev/null | grep -q 'HEAD is now at'"
check "post-checkout hook ran with git's arguments" bash -c "read -r old new flag < '$tmp/hook-ran' && [[ \$old =~ ^0+\$ && \$new == \$(git -C '$tmp/w-out' rev-parse HEAD) && \$flag == 1 ]]"
check "-B resets branch at commit" bash -c "git worktree add -q -B feat-b ../w-b HEAD~1 && [[ \$(git -C '$tmp/w-b' rev-parse HEAD) == \$(git rev-parse HEAD~1) ]] && clean '$tmp/w-b'"
check "--lock --reason" bash -c "git worktree add -q --lock --reason agent ../w-lock && git worktree list --porcelain | grep -q 'locked agent' && git worktree unlock ../w-lock"
check "-C from elsewhere with relative path" bash -c "cd '$tmp' && git -C repo worktree add -q ../w-c && clean '$tmp/w-c' && carried '$tmp/w-c'"
check "--no-checkout passes through" bash -c "git worktree add -q --no-checkout ../w-nc && [[ \$(ls -A '$tmp/w-nc') == .git ]]"
git worktree add -h 2>&1 | grep -q -- --orphan && check "--orphan passes through" bash -c "git worktree add -q --orphan -b orph ../w-orph && [[ \$(git -C '$tmp/w-orph' branch --show-current) == orph ]]"
check "populate failure falls back to git checkout" bash -c "GIT_COW_BIN=false git worktree add -q ../w-fb 2>/dev/null && clean '$tmp/w-fb' && test -f '$tmp/w-fb/README'"
check "GIT_COW_DISABLE=1 is plain git" bash -c "GIT_COW_DISABLE=1 git worktree add -q ../w-plain && test ! -e '$tmp/w-plain/node_modules'"
check "existing path fails like git" fails git worktree add -q ../w-out
for w in w-out w-b w-lock w-c w-nc w-orph w-fb w-plain; do [[ ! -e $tmp/$w ]] || git worktree remove --force --force "$tmp/$w"; done
rm "$main/.git/hooks/post-checkout"

echo "--- worktree management (real git)"
check "checked-out branch protected" fails git -C "$main" switch -q feat
check "branch -D of checked-out branch refused" fails git -C "$main" branch -D feat
check "lock / unlock" bash -c "git worktree lock '$wt' && git worktree unlock '$wt'"
# git refuses to move/remove worktrees with initialised submodules (without --force),
# so use a fresh one for move/remove
check "worktree add for move" git worktree add -q -b mv ../mv
check "move" git worktree move "$tmp/mv" "$tmp/mv-moved"
check "status after move" clean "$tmp/mv-moved"
check "commit after move" bash -c "echo m > '$tmp/mv-moved/m2.txt' && git -C '$tmp/mv-moved' add m2.txt && git -C '$tmp/mv-moved' commit -qm moved"
check "remove" git worktree remove "$tmp/mv-moved"
check "remove --force (submodules)" git worktree remove --force "$wt"
check "remove --force dirty" bash -c "echo d >> '$old/README' && git worktree remove --force '$old'"
check "prune leaves only main" bash -c "git worktree prune && [[ \$(git worktree list | wc -l) -eq 1 ]]"
check "gc in main" git -C "$main" gc -q

echo
if ((failures)); then echo "$failures FAILED"; exit 1; fi
echo "all e2e checks passed"
