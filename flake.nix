{
  description = "git worktrees backed by copy-on-write clones (APFS, btrfs, XFS, ZFS, ...)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        git-cow = pkgs.callPackage ./package.nix { };
        default = git-cow;
      });
      overlays.default = final: _prev: {
        git-cow = final.callPackage ./package.nix { };
      };
    };
}
