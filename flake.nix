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
      # Single source of truth for package name and version.
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      pname = cargoToml.package.name;
      version = cargoToml.package.version;

      # Overlay is system-agnostic — must live outside eachDefaultSystem.
      overlay = final: _prev: {
        ${pname} = self.packages.${final.system}.default;
      };
    in
    flake-utils.lib.eachDefaultSystem
      (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Stable Rust + wasm32-wasip1 target — the minimum the package build needs.
        buildToolchain = with fenix.packages.${system}; combine [
          stable.cargo
          stable.rustc
          targets.wasm32-wasip1.stable.rust-std
        ];

        # Full dev toolchain incl. clippy/rustfmt/rust-analyzer for the devShell.
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

        # Filtered source — only relevant build inputs, no artefacts
        # (`result` symlink, `target/`, etc.) pulled into the Nix store.
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

          # Plugin is WASM — no native binary, no `cargo test` across targets.
          doCheck = false;

          buildPhase = ''
            runHook preBuild
            # --frozen implies --offline and --locked: no network access,
            # Cargo.lock must not be modified → strictly reproducible.
            cargo build --release --target wasm32-wasip1 --frozen
            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall
            mkdir -p $out/bin
            # Canonical name + short alias symlink (Zellij plugin namespace).
            install -Dm644 target/wasm32-wasip1/release/${pname}.wasm \
              $out/bin/${pname}.wasm
            ln -s ${pname}.wasm $out/bin/tab-quicksearch.wasm
            runHook postInstall
          '';

          meta = with pkgs.lib; {
            description = cargoToml.package.description or "Zellij fuzzy tab picker";
            longDescription = ''
              A Zellij plugin providing a floating, fuzzy-matching, typo-tolerant
              tab picker.
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
          # `cargo test` compiles the crate (and thus zellij-tile ->
          # zellij-utils) for the host, which links OpenSSL; pkg-config locates
          # it at build time, LD_LIBRARY_PATH lets the test binary find
          # libssl/libcrypto at run time. The wasm build needs neither. Lets
          # `nix develop -c cargo test` run the unit tests out of the box.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.openssl ];
          packages = [
            devToolchain
            pkgs.zellij # for testing the built WASM in a real plugin host
            pkgs.wabt # wasm-objdump etc. for WASM inspection
            pkgs.nixpkgs-fmt # consistent with the flake formatter
          ];
        };
      })
    // {
      overlays.default = overlay;
    };
}
