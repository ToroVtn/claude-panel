//! Linux app-menu entry (icon + .desktop). Package managers (AUR, Homebrew) and the Windows .msi ship
//! their own; the install script and `cargo install` can't run a post-install step, so we add it on
//! first run instead, and `claude-panel --uninstall` takes everything back out.

use std::fs;
use std::path::{Path, PathBuf};

const DESKTOP: &str = include_str!("../assets/claude-panel.desktop");
const ICON: &str = include_str!("../assets/claude-panel.svg");

fn files() -> [PathBuf; 2] {
    let share = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| crate::data::home().join(".local/share"));
    [share.join("applications/claude-panel.desktop"), share.join("icons/hicolor/scalable/apps/claude-panel.svg")]
}

/// Under /usr, /opt or a Homebrew Cellar: a package manager owns us, our menu entry and our removal.
fn packaged(exe: &Path) -> bool {
    exe.starts_with("/usr") || exe.starts_with("/opt") || exe.components().any(|c| c.as_os_str() == "Cellar")
}

/// Absolute path, since menus don't get the shell's PATH; TryExec hides the entry if the binary is deleted.
fn desktop_entry(exe: &Path) -> String {
    DESKTOP.replace("Exec=claude-panel\n", &format!("TryExec={0}\nExec=\"{0}\"\n", exe.display()))
}

pub fn install() {
    if cfg!(any(windows, target_os = "macos", debug_assertions)) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    if packaged(&exe) {
        return;
    }
    for (path, text) in files().iter().zip([desktop_entry(&exe).as_str(), ICON]) {
        if fs::read_to_string(path).ok().as_deref() != Some(text) {  // also refreshes it if the binary moved
            let _ = path.parent().map(fs::create_dir_all);
            let _ = fs::write(path, text);
        }
    }
}

pub fn uninstall() {
    for path in files() {
        if fs::remove_file(&path).is_ok() {
            println!("Removed {}", path.display());
        }
    }
    let Ok(exe) = std::env::current_exe() else { return };
    if packaged(&exe) {
        println!("{} belongs to your package manager; remove it there (pacman -R claude-panel, brew uninstall claude-panel).", exe.display());
    } else if cfg!(windows) {
        println!("Remove Claude Panel from Settings > Apps, or delete {} if you didn't use the .msi.", exe.display());
    } else {
        match fs::remove_file(&exe) {  // fine on unix while running: the open file lives until we exit
            Ok(()) => println!("Removed {}", exe.display()),
            Err(e) => println!("Couldn't remove {} ({e}); delete it by hand.", exe.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_points_at_the_binary() {
        let text = desktop_entry(Path::new("/home/me/.cargo/bin/claude-panel"));
        assert!(text.contains("\nTryExec=/home/me/.cargo/bin/claude-panel\nExec=\"/home/me/.cargo/bin/claude-panel\"\n"));
        assert!(!text.contains("Exec=claude-panel"));
        assert!(packaged(Path::new("/usr/bin/claude-panel")) && !packaged(Path::new("/home/me/.cargo/bin/claude-panel")));
    }
}
