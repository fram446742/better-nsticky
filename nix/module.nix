{ lib, pkgs, config, ... }:

let
  inherit (lib)
  mkOption
  mkEnableOption
  types
  mkIf
  literalExpression
  ;

  cfg = config.programs.nsticky;
  tomlFormat = pkgs.formats.toml { };
in
{
  options.programs.nsticky = {
    enable = mkEnableOption "nsticky";
    package = mkOption {
      type = with types; nullOr package;
      default = pkgs.callPackage ./package.nix { };
      description = ''
        The nsticky package to use.
      '';
    };

    menu = mkOption {
      type = with types; nullOr (either str (listOf str));
      default = null;
      example = literalExpression ''"vicinae dmenu --placeholder 'Restore Window:'"'';
      description = ''
        Command used by `stage restore` to pick staged window(s). A string is
        split with shell-style quoting (it is never executed through a shell);
        a list is used verbatim as argv. [Vicinae](https://www.vicinae.com/) is
        the recommended selector; any dmenu-compatible program works.
        Overrides the `menu` key in `settings`. When null, nsticky discovers a
        selector on `PATH` and otherwise falls back to the built-in terminal
        prompt.
      '';
    };

    settings = mkOption {
      inherit (tomlFormat) type;
      default = { };
      example = literalExpression ''
        {
          menu = "vicinae dmenu --placeholder 'Restore Window:'";
          stage-workspace = "stage";
          scratchpad-workspace = "scratchpad";

          sticky = {
            firefox."app-id" = "firefox";
            kitty = {
              "app-id" = "kitty";
              title = ".*server.*";
            };
            gmail.title = ".*Gmail.*";
          };

          stage.games."app-id" = [ "steam_app_.*" "^lutris$" ];

          scratchpad.term = {
            "app-id" = "foot";
            title = "dropdown-terminal";
            spawn = [ "foot" "--app-id" "foot" "--title" "dropdown-terminal" ];
          };
        }
      '';
      description = ''
        Configuration written to
        {file}`$XDG_CONFIG_HOME/nsticky/config.toml`. Keys and rule fields are
        the ones the TOML file uses, quoted as Nix requires (`"app-id"`,
        `"exclude-title"`, …); the repository README documents all of them. The
        `menu` option above wins over `menu` set here.
      '';
    };
  };

  config = mkIf cfg.enable {
    home.packages = mkIf (cfg.package != null) [
      cfg.package
    ];

    xdg.configFile."nsticky/config.toml" = mkIf (cfg.settings != { } || cfg.menu != null) {
      source = tomlFormat.generate "sticky-config" (cfg.settings // lib.optionalAttrs (cfg.menu != null) { menu = cfg.menu; });
    };

    systemd.user.services.nsticky = {
      Unit = {
        Description = "nsticky service";
        PartOf = [ config.wayland.systemd.target ];
        After = [ config.wayland.systemd.target ];
      };

      Service = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package}";
        Restart = "on-failure";
      };

      Install.WantedBy = [ config.wayland.systemd.target ];
    };
  };
}
