# devenv module: replaces `git` in the shell with the git-cow wrapper, so
# `git worktree add` (by you, scripts or coding agents) creates copy-on-write worktrees.
#
#   # devenv.yaml
#   inputs:
#     git-cow:
#       url: github:onnimonni/git-cow
#       flake: false
#   imports:
#     - git-cow/devenv-module
{
  pkgs,
  lib,
  config,
  ...
}:

let
  cfg = config.git-cow;
in
{
  options.git-cow = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Replace `git` with the git-cow wrapper (copy-on-write `git worktree add`).";
    };
    git = lib.mkOption {
      type = lib.types.package;
      default = pkgs.git;
      defaultText = lib.literalExpression "pkgs.git";
      description = "The real git the wrapper runs.";
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../package.nix { git = cfg.git; };
      defaultText = lib.literalExpression "pkgs.callPackage ../package.nix { git = config.git-cow.git; }";
      description = "The git-cow package.";
    };
  };

  config = lib.mkIf cfg.enable {
    # hiPrio: our bin/git wins over git itself (which still provides git-upload-pack etc.)
    packages = [
      (lib.hiPrio cfg.package)
      cfg.git
    ];
  };
}
