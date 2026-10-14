{
  description = "Covalence development environment";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      toolchain = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
    in {
      devShells = nixpkgs.lib.genAttrs systems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = assert pkgs.rustc.version == toolchain; pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy rust-analyzer git direnv python313 python313Packages.mypy maturin ];
            PYO3_PYTHON = "${pkgs.python313}/bin/python3";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            shellHook = ''
              export PATH="$(git rev-parse --show-toplevel)/bin:$PATH"
            '';
          };
        });
    };
}
