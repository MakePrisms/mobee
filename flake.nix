{
  description = "Mobee";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.runCommand "mobee-0.1.0" {
            nativeBuildInputs = [
              pkgs.cargo
              pkgs.rustc
              pkgs.stdenv.cc
            ];
            src = self;

            meta = {
              description = "Mobee";
              mainProgram = "mobee";
            };
          } ''
            cp -r "$src" source
            chmod -R u+w source
            cd source
            export CARGO_HOME="$TMPDIR/cargo-home"
            cargo build --release --locked --offline
            mkdir -p "$out/bin"
            cp target/release/mobee "$out/bin/"
          '';
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/mobee";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
            ];
          };
        }
      );
    };
}
