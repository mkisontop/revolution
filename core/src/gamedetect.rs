//! Game-identity detection ladder (DESIGN.md §5.5) — the pure parts.
//!
//! The Windows shell supplies the foreground exe path (Win32) and the Steam
//! library folders; everything here — Discord detectable-DB matching and
//! Valve appmanifest parsing — is platform-independent and tested.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DetectableEntry {
    pub name: String,
    #[serde(default)]
    pub executables: Vec<String>,
}

/// Lower-cased final path component.
pub fn exe_file_name(path: &str) -> String {
    let norm = path.replace('\\', "/").to_lowercase();
    norm.rsplit('/').next().unwrap_or(&norm).to_string()
}

/// Match a foreground exe against the (cached) Discord detectable-games DB.
/// Entries may list bare exe names or relative paths.
pub fn match_exe<'a>(db: &'a [DetectableEntry], exe_path: &str) -> Option<&'a DetectableEntry> {
    let file = exe_file_name(exe_path);
    let full = exe_path.replace('\\', "/").to_lowercase();
    db.iter().find(|e| {
        e.executables.iter().any(|x| {
            let x = x.replace('\\', "/").to_lowercase();
            x == file || x.ends_with(&format!("/{file}")) || full.ends_with(&x)
        })
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct SteamApp {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
}

/// Parse a Steam `appmanifest_*.acf` (Valve KeyValues text format).
pub fn parse_appmanifest(text: &str) -> Option<SteamApp> {
    let mut appid: Option<u32> = None;
    let mut name: Option<String> = None;
    let mut installdir: Option<String> = None;
    for line in text.lines() {
        let q = quoted_strings(line);
        if q.len() >= 2 {
            match q[0].to_lowercase().as_str() {
                "appid" => appid = q[1].parse().ok(),
                "name" => name = Some(q[1].clone()),
                "installdir" => installdir = Some(q[1].clone()),
                _ => {}
            }
        }
    }
    Some(SteamApp { appid: appid?, name: name?, installdir: installdir? })
}

/// Does a running exe path belong to a Steam install dir? (Ladder step 3.)
pub fn exe_in_installdir(exe_path: &str, library_common_dir: &str, installdir: &str) -> bool {
    let exe = exe_path.replace('\\', "/").to_lowercase();
    let prefix = format!(
        "{}/{}/",
        library_common_dir.replace('\\', "/").trim_end_matches('/').to_lowercase(),
        installdir.to_lowercase()
    );
    exe.starts_with(&prefix)
}

fn quoted_strings(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => {
                if in_quotes {
                    out.push(std::mem::take(&mut cur));
                }
                in_quotes = !in_quotes;
            }
            _ if in_quotes => cur.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
"AppState"
{
	"appid"		"1623730"
	"Universe"		"1"
	"name"		"Palworld"
	"StateFlags"		"4"
	"installdir"		"Palworld"
	"LastUpdated"		"1721400000"
	"SizeOnDisk"		"22000000000"
}
"#;

    #[test]
    fn parses_real_shaped_appmanifest() {
        let app = parse_appmanifest(MANIFEST).expect("parse");
        assert_eq!(
            app,
            SteamApp { appid: 1_623_730, name: "Palworld".into(), installdir: "Palworld".into() }
        );
    }

    #[test]
    fn rejects_incomplete_manifest() {
        assert!(parse_appmanifest("\"AppState\" { \"appid\" \"1\" }").is_none());
    }

    #[test]
    fn matches_exe_against_detectable_db() {
        let db = vec![
            DetectableEntry {
                name: "Palworld".into(),
                executables: vec!["palworld.exe".into(), "pal/binaries/win64/palworld-win64-shipping.exe".into()],
            },
            DetectableEntry { name: "Elden Ring".into(), executables: vec!["eldenring.exe".into()] },
        ];
        let hit = match_exe(&db, r"D:\SteamLibrary\steamapps\common\Palworld\Pal\Binaries\Win64\Palworld-Win64-Shipping.exe");
        assert_eq!(hit.map(|e| e.name.as_str()), Some("Palworld"));
        let hit = match_exe(&db, r"C:\Games\ELDEN RING\Game\eldenring.exe");
        assert_eq!(hit.map(|e| e.name.as_str()), Some("Elden Ring"));
        assert!(match_exe(&db, r"C:\Windows\notepad.exe").is_none());
    }

    #[test]
    fn steam_installdir_prefix_matching() {
        assert!(exe_in_installdir(
            r"D:\SteamLibrary\steamapps\common\Palworld\Pal\Binaries\Win64\Palworld-Win64-Shipping.exe",
            r"D:\SteamLibrary\steamapps\common",
            "Palworld"
        ));
        assert!(!exe_in_installdir(
            r"D:\SteamLibrary\steamapps\common\PalworldModKit\tool.exe",
            r"D:\SteamLibrary\steamapps\common",
            "Palworld"
        ));
    }
}
