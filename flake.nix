{
  description = "Zellij plugin: fuzzy tab switcher with frequency-based sort (WASM)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self
    , nixpkgs
    , flake-utils
    , fenix
    ,
    }:
    let
      # Single-Source-of-Truth für Name und Version.
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      pname = cargoToml.package.name;
      version = cargoToml.package.version;

      # Overlay ist system-unabhängig — muss außerhalb von eachDefaultSystem stehen.
      overlay = final: _prev: {
        ${pname} = self.packages.${final.system}.default;
      };
    in
    flake-utils.lib.eachDefaultSystem
      (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Stable-Rust + wasm32-wasip1 Target — das Minimum, das der Package-Build braucht.
        buildToolchain = with fenix.packages.${system}; combine [
          stable.cargo
          stable.rustc
          targets.wasm32-wasip1.stable.rust-std
        ];

        # Volle Dev-Toolchain inkl. clippy/rustfmt/rust-analyzer für die devShell.
        devToolchain = with fenix.packages.${system}; combine [
          stable.cargo
          stable.rustc
          stable.clippy
          stable.rustfmt
          stable.rust-analyzer
          targets.wasm32-wasip1.stable.rust-std
        ];

        rustPlatform = pkgs.makeRustPlatform {
          cargo = buildToolchain;
          rustc = buildToolchain;
        };

        # Gefilterte Quelle — nur relevante Build-Inputs, keine Artefakte
        # (`result`-Symlink, `target/`, etc.) in den Nix-Store ziehen.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            let rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
            in !(
              (type == "symlink" && pkgs.lib.hasPrefix "result" rel)
              || pkgs.lib.hasPrefix "target" rel
              || pkgs.lib.hasPrefix ".direnv" rel
            ) && (pkgs.lib.cleanSourceFilter path type);
        };

        package = rustPlatform.buildRustPackage {
          inherit pname version src;
          cargoLock.lockFile = ./Cargo.lock;

          # Plugin ist WASM — kein nativer Binary, kein `cargo test` über Ziele.
          doCheck = false;

          buildPhase = ''
            runHook preBuild
            # --frozen impliziert --offline und --locked: kein Netzwerk,
            # Cargo.lock darf nicht modifiziert werden → strikt reproduzierbar.
            cargo build --release --target wasm32-wasip1 --frozen
            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall
            mkdir -p $out/bin
            # Kanonischer Name + kurzer Alias-Symlink (Zellij-Plugin-Namensraum).
            install -Dm644 target/wasm32-wasip1/release/${pname}.wasm \
              $out/bin/${pname}.wasm
            ln -s ${pname}.wasm $out/bin/tab-quicksearch.wasm
            runHook postInstall
          '';

          meta = with pkgs.lib; {
            description = cargoToml.package.description or "Zellij fuzzy tab picker";
            longDescription = ''
              A Zellij plugin providing a floating, fuzzy-matching tab picker.
              Features frequency-based ordering, instant jump via digit keys,
              theme-aware rendering, and graceful behaviour on narrow panes.
            '';
            license = licenses.mit;
            platforms = platforms.all;
            sourceProvenance = with sourceTypes; [ fromSource ];
            maintainers = [{
              name = "Christian Wohlert";
              email = "christian667@gmail.com";
            }];
          };
        };
      in
      {
        packages = {
          default = package;
          ${pname} = package;
        };

        checks.default = package;

        formatter = pkgs.nixpkgs-fmt;

        devShells.default = pkgs.mkShell {
          packages = [
            devToolchain
            pkgs.zellij # zum Testen der gebauten WASM im echten Plugin-Host
            pkgs.wabt # wasm-objdump etc. für WASM-Inspection
            pkgs.nixpkgs-fmt # konsistent mit flake-Formatter
          ];
        };
      })
    // {
      overlays.default = overlay;
    };
}
