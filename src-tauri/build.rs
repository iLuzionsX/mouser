// Icon generation is delegated to `tauri icon`.
//
// The PNG/ICO/ICNS formats are awkward to keep correct by hand, and the Tauri
// CLI already ships a converter. The source of truth is icon.svg at the repo
// root; regenerate the set with:
//
//     cargo tauri icon icon.svg
//
// Nothing is written at build time: the generated files are committed, so a
// clean checkout builds without needing the CLI.

fn main() {
    // Rebuild if the Tauri config changes, since the icon list lives there.
    println!("cargo:rerun-if-changed=tauri.conf.json");
    tauri_build::build()
}
