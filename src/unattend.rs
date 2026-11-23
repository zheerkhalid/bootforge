//! The Windows Setup answer file (`autounattend.xml`) BootForge can add to a
//! drive made from a Windows setup image.
//!
//! Windows Setup looks for a file of this name in the root of the media it
//! was started from and follows it. Nothing inside the image is changed: the
//! file only sets registry values Setup itself reads and standard answer-file
//! settings.
//!
//! * **Requirement checks** (Windows 11): `HKLM\SYSTEM\Setup\LabConfig` values
//!   set in the `windowsPE` pass, before Setup evaluates the PC.
//! * **Local account**: the `BypassNRO` value set in the `specialize` pass plus
//!   `HideOnlineAccountScreens`, so the first start asks for a local user
//!   name instead of a Microsoft account.
//! * **Privacy questions**: `ProtectYourPC` = 3, the "no" answer to all of them.

use crate::model::{Arch, SetupTweaks};

/// Name of the file in the root of the drive.
pub const FILE_NAME: &str = "autounattend.xml";

const COMPONENT_ATTRS: &str =
    "publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\"";

/// The registry values that switch off the hardware checks of Windows 11 Setup.
pub const LAB_CONFIG_VALUES: [&str; 3] =
    ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck"];

/// The answer-file name of a CPU architecture (`None`: Windows Setup has no
/// such architecture).
fn arch_name(arch: Arch) -> Option<&'static str> {
    match arch {
        Arch::X64 => Some("amd64"),
        Arch::X86 => Some("x86"),
        Arch::Arm64 => Some("arm64"),
        Arch::Arm | Arch::Ia64 | Arch::RiscV64 => None,
    }
}

/// The architectures the file is written for: those of the image, or x64
/// when the image does not say.
fn arch_names(archs: &[Arch]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = archs.iter().filter_map(|a| arch_name(*a)).collect();
    names.dedup();
    if names.is_empty() {
        names.push("amd64");
    }
    names
}

fn run_commands(out: &mut String, commands: &[String]) {
    out.push_str("      <RunSynchronous>\r\n");
    for (i, command) in commands.iter().enumerate() {
        out.push_str("        <RunSynchronousCommand wcm:action=\"add\">\r\n");
        out.push_str(&format!("          <Order>{}</Order>\r\n", i + 1));
        out.push_str(&format!("          <Path>{command}</Path>\r\n"));
        out.push_str("        </RunSynchronousCommand>\r\n");
    }
    out.push_str("      </RunSynchronous>\r\n");
}

fn component(out: &mut String, name: &str, arch: &str, body: impl FnOnce(&mut String)) {
    out.push_str(&format!(
        "    <component name=\"{name}\" processorArchitecture=\"{arch}\" {COMPONENT_ATTRS}>\r\n"
    ));
    body(out);
    out.push_str("    </component>\r\n");
}

/// Builds the answer file for the chosen tweaks. `None` when nothing is chosen.
pub fn build(tweaks: &SetupTweaks, archs: &[Arch]) -> Option<String> {
    if !tweaks.any() {
        return None;
    }
    let archs = arch_names(archs);
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n");
    x.push_str(
        "<!-- Written by BootForge. Delete this file to get the unchanged Windows Setup. -->\r\n",
    );
    x.push_str(
        "<unattend xmlns=\"urn:schemas-microsoft-com:unattend\" xmlns:wcm=\"http://schemas.microsoft.com/WMIConfig/2002/State\">\r\n",
    );
    if tweaks.skip_requirements {
        let commands: Vec<String> = LAB_CONFIG_VALUES
            .iter()
            .map(|v| format!("reg add HKLM\\SYSTEM\\Setup\\LabConfig /v {v} /t REG_DWORD /d 1 /f"))
            .collect();
        x.push_str("  <settings pass=\"windowsPE\">\r\n");
        for arch in &archs {
            component(&mut x, "Microsoft-Windows-Setup", arch, |x| {
                run_commands(x, &commands)
            });
        }
        x.push_str("  </settings>\r\n");
    }
    if tweaks.local_account {
        let commands = vec![
            "reg add HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\OOBE /v BypassNRO /t REG_DWORD /d 1 /f"
                .to_string(),
        ];
        x.push_str("  <settings pass=\"specialize\">\r\n");
        for arch in &archs {
            component(&mut x, "Microsoft-Windows-Deployment", arch, |x| {
                run_commands(x, &commands)
            });
        }
        x.push_str("  </settings>\r\n");
    }
    if tweaks.local_account || tweaks.skip_privacy {
        x.push_str("  <settings pass=\"oobeSystem\">\r\n");
        for arch in &archs {
            component(&mut x, "Microsoft-Windows-Shell-Setup", arch, |x| {
                x.push_str("      <OOBE>\r\n");
                if tweaks.local_account {
                    x.push_str(
                        "        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>\r\n",
                    );
                }
                if tweaks.skip_privacy {
                    x.push_str("        <ProtectYourPC>3</ProtectYourPC>\r\n");
                }
                x.push_str("      </OOBE>\r\n");
            });
        }
        x.push_str("  </settings>\r\n");
    }
    x.push_str("</unattend>\r\n");
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small well-formedness check: every opened element is closed in order.
    fn assert_well_formed(xml: &str) {
        let mut stack: Vec<String> = Vec::new();
        let mut rest = xml;
        while let Some(start) = rest.find('<') {
            let end = start + rest[start..].find('>').expect("unterminated tag");
            let tag = &rest[start + 1..end];
            rest = &rest[end + 1..];
            if tag.starts_with('?') || tag.starts_with('!') {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                assert_eq!(stack.pop().as_deref(), Some(name), "closing {name}");
            } else if !tag.ends_with('/') {
                let name = tag.split_whitespace().next().unwrap();
                // Attribute values are quoted and none contains '<' or '>'.
                assert_eq!(tag.matches('"').count() % 2, 0, "{tag}");
                stack.push(name.to_string());
            }
        }
        assert!(stack.is_empty(), "unclosed: {stack:?}");
        assert!(!rest.contains('>'));
    }

    #[test]
    fn nothing_chosen_means_no_file() {
        assert_eq!(build(&SetupTweaks::default(), &[Arch::X64]), None);
    }

    #[test]
    fn each_tweak_adds_only_its_own_settings() {
        let only = |f: fn(&mut SetupTweaks)| {
            let mut t = SetupTweaks::default();
            f(&mut t);
            let xml = build(&t, &[Arch::X64]).unwrap();
            assert_well_formed(&xml);
            xml
        };
        let req = only(|t| t.skip_requirements = true);
        for v in LAB_CONFIG_VALUES {
            assert!(req.contains(&format!("LabConfig /v {v} /t REG_DWORD /d 1 /f")));
        }
        assert!(req.contains("pass=\"windowsPE\""));
        assert!(!req.contains("specialize") && !req.contains("oobeSystem"));
        assert!(req.contains("<Order>3</Order>") && !req.contains("<Order>4</Order>"));

        let local = only(|t| t.local_account = true);
        assert!(local.contains("BypassNRO"));
        assert!(local.contains("<HideOnlineAccountScreens>true</HideOnlineAccountScreens>"));
        assert!(!local.contains("LabConfig") && !local.contains("ProtectYourPC"));

        let privacy = only(|t| t.skip_privacy = true);
        assert!(privacy.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(!privacy.contains("BypassNRO") && !privacy.contains("HideOnlineAccountScreens"));
        assert!(!privacy.contains("RunSynchronous"));
    }

    #[test]
    fn everything_together_is_one_well_formed_document() {
        let all = SetupTweaks {
            skip_requirements: true,
            local_account: true,
            skip_privacy: true,
        };
        let xml = build(&all, &[Arch::X64]).unwrap();
        assert_well_formed(&xml);
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("xmlns=\"urn:schemas-microsoft-com:unattend\""));
        assert!(xml.trim_end().ends_with("</unattend>"));
        assert_eq!(xml.matches("<settings ").count(), 3);
        assert_eq!(xml.matches("processorArchitecture=\"amd64\"").count(), 3);
        // Plain ASCII with Windows line ends: readable in Notepad on any system.
        assert!(xml.is_ascii());
        assert_eq!(xml.matches('\n').count(), xml.matches("\r\n").count());
    }

    #[test]
    fn architectures_follow_the_image() {
        let t = SetupTweaks {
            skip_privacy: true,
            ..Default::default()
        };
        let arm = build(&t, &[Arch::Arm64]).unwrap();
        assert!(arm.contains("processorArchitecture=\"arm64\"") && !arm.contains("amd64"));
        let both = build(&t, &[Arch::X86, Arch::X64]).unwrap();
        assert_well_formed(&both);
        assert!(both.contains("\"x86\"") && both.contains("\"amd64\""));
        assert_eq!(both.matches("<component ").count(), 2);
        // Unknown or unsupported architectures fall back to x64.
        for archs in [&[][..], &[Arch::RiscV64][..]] {
            let xml = build(&t, archs).unwrap();
            assert_eq!(xml.matches("processorArchitecture=\"amd64\"").count(), 1);
        }
    }
}
