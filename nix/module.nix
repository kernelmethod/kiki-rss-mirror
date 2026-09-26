{ config, lib, pkgs, ... }:

let
  cfg = config.services.kiki;
in
{
  options.services.kiki = {
    enable = lib.mkEnableOption "kiki RSS feed aggregator";

    package = lib.mkPackageOption pkgs "kiki" { };

    port = lib.mkOption {
      type = lib.types.nullOr lib.types.port;
      default = null;
      description = "TCP port to listen on. Mutually exclusive with unixSocket.";
    };

    unixSocket = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = "/run/kiki/kiki.sock";
      description = "Path to Unix domain socket. Set to null to use TCP port instead.";
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
    assertions = [{
      assertion = !(cfg.port != null && cfg.unixSocket != null);
      message = "services.kiki: port and unixSocket are mutually exclusive.";
    }];

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
        RuntimeDirectory = lib.mkIf (cfg.unixSocket != null) "kiki";
        ExecStartPre = "${cfg.package}/bin/kiki init --check ${cfg.dataDir}";
        ExecStart =
          let
            listenFlag =
              if cfg.port != null then "-p ${toString cfg.port}"
              else if cfg.unixSocket != null then "-u ${cfg.unixSocket}"
              else "";
          in
          "${cfg.package}/bin/kiki serve ${listenFlag}";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };
  };
}
