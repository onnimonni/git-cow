{
  lib,
  rustPlatform,
  git,
}:

rustPlatform.buildRustPackage {
  pname = "git-cow";
  version = (lib.importTOML ./Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./tests/populate.rs
      ./wrapper
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  # `git` wrapper: `git worktree add` gets copy-on-write clones, everything else is
  # the real git (resolved at build time, no PATH search).
  postInstall = ''
    install -Dm755 wrapper/git $out/bin/git
    substituteInPlace $out/bin/git \
      --replace-fail 'real_git=''${GIT_COW_REAL_GIT:-}' 'real_git=''${GIT_COW_REAL_GIT:-${git}/bin/git}' \
      --replace-fail '"''${GIT_COW_BIN:-git-cow}"' "\"\''${GIT_COW_BIN:-$out/bin/git-cow}\""
  '';

  meta = {
    description = "git worktrees backed by copy-on-write clones (APFS, btrfs, XFS, ZFS, ...)";
    homepage = "https://github.com/onnimonni/git-cow";
    license = lib.licenses.mit;
    mainProgram = "git-cow";
    platforms = lib.platforms.unix;
  };
}
