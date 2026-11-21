//! Installed games on this PC (Steam and Epic), so a guest can start one from the session menu.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// (name, launch link) for every installed Steam / Epic game, sorted by name.
pub fn installed() -> Vec<(String, String)> {
    let mut v = steam();
    v.extend(epic());
    v.sort_by_key(|g| g.0.to_lowercase());
    v.dedup_by(|a, b| a.0 == b.0);
    v
}

/// The quoted values after `key` in a Valve .vdf/.acf file.
fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let k = format!("\"{key}\"");
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix(&k)?.trim();
            Some(rest.trim_matches('"').replace("\\\\", "\\"))
        })
        .collect()
}

fn steam_root() -> Option<PathBuf> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, w!("Software\\Valve\\Steam"), w!("SteamPath"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len)) }.ok().ok()?;
    let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
    Some(PathBuf::from(s))
}

fn steam() -> Vec<(String, String)> {
    let Some(root) = steam_root() else { return Vec::new() };
    let mut libs = vec![root.clone()];
    if let Ok(t) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
        libs.extend(vdf_values(&t, "path").into_iter().map(PathBuf::from));
    }
    libs.dedup();
    let mut out = Vec::new();
    for lib in libs {
        let Ok(dir) = std::fs::read_dir(lib.join("steamapps")) else { continue };
        for e in dir.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !(n.starts_with("appmanifest_") && n.ends_with(".acf")) {
                continue;
            }
            let Ok(t) = std::fs::read_to_string(e.path()) else { continue };
            let (Some(id), Some(name)) = (vdf_values(&t, "appid").into_iter().next(), vdf_values(&t, "name").into_iter().next()) else { continue };
            // Not games: redistributables, runtimes, tools.
            let low = name.to_lowercase();
            if id == "228980" || low.contains("redistributable") || low.contains("proton") || low.contains("steamvr") || low.contains("steam linux runtime") {
                continue;
            }
            out.push((name, format!("steam://rungameid/{id}")));
        }
    }
    out
}

fn epic() -> Vec<(String, String)> {
    let dir = Path::new("C:\\ProgramData\\Epic\\EpicGamesLauncher\\Data\\Manifests");
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "item"))
        .filter_map(|e| {
            let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            let name = j["DisplayName"].as_str()?.to_string();
            let (ns, item, app) = (j["CatalogNamespace"].as_str()?, j["CatalogItemId"].as_str()?, j["AppName"].as_str()?);
            Some((name, format!("com.epicgames.launcher://apps/{ns}%3A{item}%3A{app}?action=launch&silent=true")))
        })
        .collect()
}

/// Starts a game by name (only names from `installed()`: a guest can't run arbitrary links).
pub fn launch(name: &str) -> Result<(), String> {
    let (_, url) = installed().into_iter().find(|g| g.0 == name).ok_or("That game isn't installed here")?;
    std::process::Command::new("explorer.exe").arg(url).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn vdf_parse() {
        let t = "\"AppState\"\n{\n\t\"appid\"\t\t\"570\"\n\t\"name\"\t\t\"Dota 2\"\n\t\"path\"\t\t\"D:\\\\Games\\\\Steam\"\n}";
        assert_eq!(super::vdf_values(t, "appid"), vec!["570"]);
        assert_eq!(super::vdf_values(t, "name"), vec!["Dota 2"]);
        assert_eq!(super::vdf_values(t, "path"), vec!["D:\\Games\\Steam"]);
    }
}
