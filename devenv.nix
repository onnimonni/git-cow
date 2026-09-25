{ pkgs, ... }:

{
  packages = [ pkgs.git pkgs.git-lfs ];

  languages.rust.enable = true;

  git-hooks.hooks = {
    rustfmt.enable = true;
    clippy.enable = true;
  };

  enterTest = ''
    cargo test
    cargo build --release
    tests/e2e.sh
  '';
}
