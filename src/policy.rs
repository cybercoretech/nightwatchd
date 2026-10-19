//! What the phone may do. The gateway enforces this, so a lost phone or a
//! buggy app can't go further than the token's scope and this list.
//!
//! - **read** tokens: everything that only looks.
//! - **operate** tokens add: start/stop/restart a watched unit, start/stop/
//!   restart a container that belongs to a compose stack, `up`/`restart` a
//!   stack, run a Cyberdeck scan, acknowledge alerts.
//! - never from the phone: unit-file edits and deletes, daemon-reload,
//!   polkit sync, settings, compose edits, new stacks, `down`, prune,
//!   firewall changes, integrity re-baselining.
//! - protected units (the ones that keep you connected) may be started but
//!   never stopped or restarted.

pub const UNIT_VERBS: &[&str] = &["start", "stop", "restart"];
pub const CONTAINER_VERBS: &[&str] = &["start", "stop", "restart"];
pub const STACK_VERBS: &[&str] = &["up", "restart"];

/// Units whose loss would cut the phone off from this host.
pub const BUILTIN_PROTECTED: &[&str] = &[
    "tailscaled.service",
    "sshd.service",
    "ssh.service",
    "NetworkManager.service",
    "systemd-networkd.service",
    "systemd-resolved.service",
    "iwd.service",
    "dbus.service",
    "user:dbus.service",
    "polkit.service",
    "daemonhall.service",
    "user:daemonhall.service",
    "nightwatchd.service",
    "user:nightwatchd.service",
];

/// Cyberdeck scan modules, by the name the app uses.
pub const SCANS: &[(&str, &str)] = &[
    ("audio", "RunAudioModule"),
    ("battery", "RunBatteryModule"),
    ("bios", "RunBiosModule"),
    ("cpu", "RunCpuModule"),
    ("disks", "RunDisksModule"),
    ("fan", "RunFanModule"),
    ("gpu", "RunGpuModule"),
    ("hardware", "RunHardwareModule"),
    ("kernel_modules", "RunKernelModulesModule"),
    ("memory", "RunMemoryModule"),
    ("motherboard", "RunMotherboardModule"),
    ("mounts", "RunMountsModule"),
    ("network", "RunNetworkModule"),
    ("power", "RunPowerModule"),
    ("services", "RunServicesModule"),
    ("storage", "RunStorageAiModule"),
    ("thermal", "RunThermalModule"),
    ("usb", "RunUsbModule"),
];

pub struct Policy {
    protected: Vec<String>,
}

impl Policy {
    pub fn new(extra: &[String]) -> Policy {
        let mut protected: Vec<String> = BUILTIN_PROTECTED.iter().map(|s| s.to_string()).collect();
        protected.extend(extra.iter().cloned());
        Policy { protected }
    }

    pub fn is_protected(&self, unit: &str) -> bool {
        self.protected.iter().any(|p| p == unit)
    }

    /// The verbs the phone may use on this unit right now.
    pub fn unit_verbs(&self, unit: &str, controllable: bool) -> Vec<&'static str> {
        if !controllable {
            return vec![];
        }
        if self.is_protected(unit) {
            return vec!["start"];
        }
        UNIT_VERBS.to_vec()
    }

    pub fn check_unit(&self, unit: &str, verb: &str) -> Result<(), String> {
        if !UNIT_VERBS.contains(&verb) {
            return Err(format!("{verb:?} is not allowed from the phone"));
        }
        if verb != "start" && self.is_protected(unit) {
            return Err(format!(
                "{unit} is protected: stopping or restarting it could cut this phone off"
            ));
        }
        Ok(())
    }
}

pub fn check_container(verb: &str) -> Result<(), String> {
    if CONTAINER_VERBS.contains(&verb) {
        Ok(())
    } else {
        Err(format!("{verb:?} is not allowed from the phone"))
    }
}

pub fn check_stack(verb: &str) -> Result<(), String> {
    if STACK_VERBS.contains(&verb) {
        Ok(())
    } else {
        Err(format!(
            "{verb:?} is not allowed from the phone (only up and restart)"
        ))
    }
}

pub fn scan_command(module: &str) -> Option<&'static str> {
    SCANS.iter().find(|(m, _)| *m == module).map(|(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_units_can_only_start() {
        let p = Policy::new(&["mything.service".into()]);
        assert!(p.check_unit("tailscaled.service", "start").is_ok());
        assert!(p.check_unit("tailscaled.service", "stop").is_err());
        assert!(p.check_unit("user:daemonhall.service", "restart").is_err());
        assert!(p.check_unit("mything.service", "stop").is_err());
        assert_eq!(p.unit_verbs("sshd.service", true), vec!["start"]);
    }

    #[test]
    fn ordinary_units_get_the_three_verbs_and_nothing_else() {
        let p = Policy::new(&[]);
        for v in ["start", "stop", "restart"] {
            assert!(p.check_unit("dockspace.service", v).is_ok());
        }
        for v in ["enable", "disable", "reload", "run-now", "delete", "edit"] {
            assert!(p.check_unit("dockspace.service", v).is_err(), "{v}");
        }
        assert!(p.unit_verbs("dockspace.service", false).is_empty());
    }

    #[test]
    fn stacks_never_go_down_from_the_phone() {
        assert!(check_stack("up").is_ok());
        assert!(check_stack("restart").is_ok());
        assert!(check_stack("down").is_err());
        assert!(check_container("restart").is_ok());
        assert!(check_container("rm").is_err());
    }

    #[test]
    fn scans_map_to_hub_commands() {
        assert_eq!(scan_command("cpu"), Some("RunCpuModule"));
        assert_eq!(scan_command("storage"), Some("RunStorageAiModule"));
        assert_eq!(scan_command("ArchiveFiles"), None);
    }
}
