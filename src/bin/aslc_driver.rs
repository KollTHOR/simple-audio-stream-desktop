//! Windows USB driver helper for ASLC Node.
//!
//! The desktop talks to the phone over AOA, which on Windows needs one WinUSB-bound interface.
//! ASLC uses a single *generic* driver rule (no per-device IDs); this helper installs or removes
//! that package. It must run elevated, so the app launches it with the `runas` verb (one UAC
//! prompt). It is intentionally a plain console tool so it can also be run by hand for support.
//!
//! Usage:
//!   aslc_driver install --inf <path\to\aslc_aoa.inf>
//!   aslc_driver remove
//!   aslc_driver status
//!
//! Running `remove` restores normal MTP/file-transfer: Windows re-binds the inbox WPD driver.

use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let code = match cmd {
        "install" => install(flag(&args, "--inf").as_deref()),
        "remove" => remove(),
        "status" => status(),
        _ => {
            eprintln!("usage: aslc_driver install --inf <aslc_aoa.inf> | remove | status");
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

fn install(inf: Option<&str>) -> i32 {
    let inf = inf.unwrap_or("driver\\aslc_aoa.inf");
    println!("Installing ASLC USB driver: {inf}");
    let (added, out) = pnputil(&["/add-driver", inf, "/install"]);
    print!("{out}");
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
    let mut ok = true;
    for name in &packages {
        println!("Removing {name} ...");
        let (good, out) = pnputil(&["/delete-driver", name, "/uninstall", "/force"]);
        print!("{out}");
        ok &= good;
    }
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
}
