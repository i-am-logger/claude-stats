{ pkgs
, config
, lib
, ...
}:
let
  cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
  packageName = cargoToml.package.name;
  packageVersion = cargoToml.package.version;
  packageDescription = cargoToml.package.description or "";
in
{
  # Set root explicitly for flake compatibility
  devenv.root = lib.mkDefault (builtins.toString ./.);

  dotenv.enable = true;
  imports = [
    ./nix/rust.nix
  ];

  # Additional packages for development
  packages = [
    pkgs.git
    pkgs.pkg-config
    pkgs.cargo-watch
    pkgs.cargo-llvm-cov
    pkgs.cargo-deny
    pkgs.cargo-bloat
    pkgs.cargo-mutants
  ];

  # Development scripts
  scripts.dev-test.exec = ''
    echo "Running tests..."
    cargo test --all-features
  '';

  scripts.dev-run.exec = ''
    echo "Running claude-stats..."
    cargo run --release
  '';

  scripts.dev-build.exec = ''
    echo "Building claude-stats..."
    cargo build --release
  '';

  scripts.dev-watch.exec = ''
    cargo watch -x run
  '';

  scripts.dev-coverage.exec = ''
    echo "Running tests with coverage..."
    cargo llvm-cov --html
    echo "Coverage report: target/llvm-cov/html/index.html"
  '';

  scripts.dev-bench.exec = ''
    echo "Running benchmarks..."
    cargo bench --bench parsing
    echo "Report: target/criterion/report/index.html"
  '';

  scripts.dev-bloat.exec = ''
    echo "Analyzing binary size..."
    cargo bloat --release --crates
  '';

  scripts.dev-mutants.exec = ''
    echo "Running mutation tests..."
    cargo mutants
  '';

  # Environment variables
  env = {
    PROJECT_NAME = "claude-stats";
    CARGO_TARGET_DIR = "./target";
  };

  # Development shell setup
  enterShell = ''
    clear
    ${pkgs.figlet}/bin/figlet "${packageName}"
    echo
    {
      ${pkgs.lib.optionalString (packageDescription != "") ''echo "• ${packageDescription}"''}
      echo -e "• \033[1mv${packageVersion}\033[0m"
      echo -e " \033[0;32m✓\033[0m Development environment ready"
    } | ${pkgs.boxes}/bin/boxes -d stone -a l -i none
    echo
    echo "Available scripts:"
    echo "  Rust Development:"
    echo "    • dev-test      - Run tests"
    echo "    • dev-run       - Run the application"
    echo "    • dev-build     - Build the application"
    echo "    • dev-watch     - Watch and auto-reload"
    echo "    • dev-coverage  - Run tests with coverage report"
    echo "    • dev-bench     - Run criterion benchmarks"
    echo "    • dev-bloat     - Analyze binary size by crate"
    echo "    • dev-mutants   - Run mutation tests"
    echo ""
  '';

  # https://devenv.sh/integrations/treefmt/
  treefmt = {
    enable = true;
    config = {
      settings.global.excludes = [
        ".devenv.flake.nix"
        ".devenv/"
      ];
      programs = {
        nixpkgs-fmt.enable = true;
        deadnix = {
          enable = true;
          no-underscore = true;
        };
        statix.enable = true;
        rustfmt = {
          enable = true;
          package = config.languages.rust.toolchainPackage;
          # Match the crate edition in Cargo.toml — otherwise treefmt's bare
          # rustfmt and `cargo fmt` disagree on import ordering and fight.
          edition = "2021";
        };
        shellcheck.enable = true;
        shfmt.enable = true;
      };
    };
  };

  # https://devenv.sh/git-hooks/
  git-hooks.settings.rust.cargoManifestPath = "./Cargo.toml";

  git-hooks.tools = {
    cargo = lib.mkForce config.languages.rust.toolchainPackage;
    clippy = lib.mkForce config.languages.rust.toolchainPackage;
    rustfmt = lib.mkForce config.languages.rust.toolchainPackage;
  };

  git-hooks.hooks = {
    treefmt.enable = true;
    clippy.enable = true;
  };

  # https://devenv.sh/tasks/
  tasks = {
    "test:fmt" = {
      exec = "treefmt --fail-on-change";
    };

    "test:clippy" = {
      exec = "cargo clippy --quiet -- -D warnings";
    };

    "test:check" = {
      exec = "cargo check --quiet";
    };

    "test:unit" = {
      exec = "cargo test --quiet";
    };
  };

  # https://devenv.sh/tests/
  enterTest = lib.mkForce "devenv tasks run test:fmt test:clippy test:check test:unit";
}
