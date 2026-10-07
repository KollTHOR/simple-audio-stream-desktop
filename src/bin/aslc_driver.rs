//! Windows USB driver helper for ASLC Node.
//!
//! The desktop talks to the phone over AOA, which on Windows needs one WinUSB-bound interface.
//! ASLC uses a single *generic* driver rule (no per-device IDs); this helper installs or removes
//! that package. It must run elevated, so the app launches it with the `runas` verb (one UAC
//! prompt). It is intentionally a plain console tool so it can also be run by hand for support.
//!
//! Usage:
//!   aslc_driver install [--inf <path\to\aslc_aoa.inf>]
//!   aslc_driver remove
//!   aslc_driver status
//!
//! Both install and remove force the phone's USB device to re-enumerate (pnputil */restart-device),
//! so the new driver takes effect (or MTP returns) without unplugging or rebooting.

use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let code = match cmd {
        "install" => install(flag(&args, "--inf").as_deref()),
        "remove" => remove(),
        "status" => status(),
        _ => {
            eprintln!("usage: aslc_driver install [--inf <aslc_aoa.inf>] | remove | status");
            2
        }
    };
    std::process::exit(code);
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn pnputil(args: &[&str]) -> (bool, String) {
    match Command::new("pnputil").args(args).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("could not run pnputil: {e}")),
    }
}

/// Parse `pnputil /enum-drivers` for published oemNN.inf packages whose original name is ours.
fn aslc_packages(enum_output: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in enum_output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Published Name:") {
            current = Some(rest.trim().to_string());
        } else if t.starts_with("Original Name:")
            && t.to_ascii_lowercase().contains("aslc_aoa.inf")
        {
            if let Some(n) = current.clone() {
                out.push(n);
            }
        }
    }
    out
}

/// All device Instance IDs found in `pnputil /enum-devices` output.
fn instance_ids(enum_output: &str) -> Vec<String> {
    enum_output
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("Instance ID:")
                .map(|r| r.trim().to_string())
        })
        .collect()
}

/// Instance IDs of devices currently bound to any of `packages` (from `/enum-devices /drivers`).
fn devices_using(enum_output: &str, packages: &[String]) -> Vec<String> {
    let pkgs: Vec<String> = packages.iter().map(|p| p.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    let mut current = String::new();
    let mut matched = false;
    for line in enum_output.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Instance ID:") {
            if matched && !current.is_empty() {
                out.push(current.clone());
            }
            current = rest.trim().to_string();
            matched = false;
        } else if let Some(rest) = t.strip_prefix("Driver Name:") {
            if pkgs.contains(&rest.trim().to_ascii_lowercase()) {
                matched = true;
            }
        }
    }
    if matched && !current.is_empty() {
        out.push(current);
    }
    out
}

/// Force a device (the phone) to re-enumerate so Windows re-runs driver selection. Restarts only
/// the parent node (`USB\VID_x&PID_y\<serial>`) so children don't fight the parent's restart.
fn restart_instances_for(vid: u16, pid: u16) {
    let (_, txt) = pnputil(&["/enum-devices"]);
    let prefix = format!("USB\\VID_{vid:04X}&PID_{pid:04X}\\").to_ascii_uppercase();
    for id in instance_ids(&txt) {
        if id.to_ascii_uppercase().starts_with(&prefix) {
            println!("Re-enumerating {id} ...");
            let _ = pnputil(&["/restart-device", &id]);
        }
    }
}

fn restart_instances(ids: &[String]) {
    for id in ids {
        println!("Re-enumerating {id} ...");
        let _ = pnputil(&["/restart-device", id]);
    }
}

fn install(inf: Option<&str>) -> i32 {
    let inf = inf.unwrap_or("driver\\aslc_aoa.inf");

    // Remember candidates first (while they still enumerate as normal MTP/ADB devices) so we can
    // re-enumerate them after the driver is added.
    let candidates: Vec<(u16, u16)> = aslc::aoa::list_receiver_devices()
        .iter()
        .map(|d| (d.vid, d.pid))
        .collect();

    println!("Installing ASLC USB driver: {inf}");
    let (added, out) = pnputil(&["/add-driver", inf, "/install"]);
    print!("{out}");

    for (vid, pid) in candidates {
        restart_instances_for(vid, pid);
    }
    let (_, scan) = pnputil(&["/scan-devices"]);
    print!("{scan}");

    if added {
        println!("Done. The ASLC USB driver is installed.");
        0
    } else {
        eprintln!("Driver install failed (are you elevated, and does the .inf + .cat exist?).");
        1
    }
}

fn remove() -> i32 {
    let (_, txt) = pnputil(&["/enum-drivers"]);
    let packages = aslc_packages(&txt);
    if packages.is_empty() {
        println!("No ASLC USB driver package is installed.");
        return 0;
    }

    // Which devices are using our package(s)? Re-enumerate them after removal so Windows binds the
    // inbox MTP/WPD driver again (this is what avoids a reboot).
    let (_, devs) = pnputil(&["/enum-devices", "/drivers"]);
    let affected = devices_using(&devs, &packages);

    let mut ok = true;
    for name in &packages {
        println!("Removing {name} ...");
        let (good, out) = pnputil(&["/delete-driver", name, "/uninstall", "/force"]);
        print!("{out}");
        ok &= good;
    }

    restart_instances(&affected);
    let (_, scan) = pnputil(&["/scan-devices"]);
    print!("{scan}");

    if ok {
        println!("Done. File transfer (MTP) is restored.");
        0
    } else {
        eprintln!("Some packages could not be removed.");
        1
    }
}

fn status() -> i32 {
    let (_, txt) = pnputil(&["/enum-drivers"]);
    let packages = aslc_packages(&txt);
    if packages.is_empty() {
        println!("not installed");
    } else {
        println!("installed: {}", packages.join(", "));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aslc_packages_from_enum_output() {
        let sample = "
Published Name:     oem55.inf
Original Name:      aslc_aoa.inf
Provider Name:      ASLC Node

Published Name:     oem12.inf
Original Name:      somethingelse.inf
";
        assert_eq!(aslc_packages(sample), vec!["oem55.inf".to_string()]);
    }

    #[test]
    fn parses_devices_using_our_package() {
        let sample = "
Instance ID:                USB\\VID_2717&PID_FF40\\744B5316
Driver Name:                oem55.inf

Instance ID:                USB\\VID_046D&PID_C539\\abc
Driver Name:                input.inf
";
        assert_eq!(
            devices_using(sample, &["oem55.inf".to_string()]),
            vec!["USB\\VID_2717&PID_FF40\\744B5316".to_string()]
        );
    }
}
