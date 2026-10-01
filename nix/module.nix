{ config, lib, pkgs, ... }:

let
  cfg = config.services.kiki;
in
{
  options.services.kiki = {
    enable = lib.mkEnableOption "kiki RSS feed aggregator";

    package = lib.mkPackageOption pkgs "kiki" { };

    unixSocket = lib.mkOption {
      type = lib.types.path;
      default = "/run/kiki/kiki.sock";
      description = ''
        Path to the Unix domain socket kiki serves on. kiki does not listen
        on TCP; put a reverse proxy in front of this socket to expose it
        over the network.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/kiki";
      description = "Directory for kiki data files.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "kiki";
      description = "User account under which kiki runs.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "kiki";
      description = "Group under which kiki runs.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.dataDir;
    };

    users.groups.${cfg.group} = { };

    systemd.tmpfiles.rules = [
      "d ${cfg.dataDir} 0750 ${cfg.user} ${cfg.group} -"
    ];

    systemd.services.kiki = {
      description = "Kiki RSS feed aggregator";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      environment.KIKI_HOME = cfg.dataDir;

      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.dataDir;
        RuntimeDirectory = "kiki";
        ExecStartPre = "${cfg.package}/bin/kiki init --check";
        ExecStart = "${cfg.package}/bin/kiki serve -u ${cfg.unixSocket}";
        Restart = "on-failure";
        RestartSec = 5;

        # Kiki refuses writable and executable memory itself on Linux 6.3+;
        # these also cover older kernels.
        MemoryDenyWriteExecute = true;
        LockPersonality = true;
      };
    };
  };
}
