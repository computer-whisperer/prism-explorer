//! Dump damascene bundle artifacts (svg, tree dump, draw ops, shader
//! manifest, lint) for every canned scene to `crates/explorer/out/`.
//!
//! The cheapest layout-review loop during UI work: CPU-only, no
//! window, same layout + draw-op stack the GPU consumes. The SVG and
//! tree dump together make layout regressions obvious; lint catches
//! raw values, overflows, and duplicate ids.
//!
//! Usage:
//!   cargo run -p prism-explorer --bin dump_bundles              # all scenes
//!   cargo run -p prism-explorer --bin dump_bundles -- grid ...  # just these
//!
//! Exits nonzero if any rendered scene has lint findings.

use std::path::PathBuf;

use damascene_core::write_bundle;
use prism_explorer::fixtures::{render, scenes};

fn main() -> std::io::Result<()> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");
    let filter: Vec<String> = std::env::args().skip(1).collect();

    let all = scenes();
    if let Some(unknown) = filter.iter().find(|f| !all.iter().any(|s| s.name == **f)) {
        let names: Vec<&str> = all.iter().map(|s| s.name).collect();
        eprintln!("unknown scene `{unknown}` — scenes: {}", names.join(", "));
        std::process::exit(2);
    }

    let mut findings = 0;
    for scene in all {
        if !filter.is_empty() && !filter.iter().any(|f| f == scene.name) {
            continue;
        }
        let bundle = render(scene.app.as_ref(), scene.viewport);
        for path in write_bundle(&bundle, &out_dir, scene.name)? {
            println!("wrote {}", path.display());
        }
        if !bundle.lint.findings.is_empty() {
            eprintln!(
                "\n[{}] lint findings ({}):",
                scene.name,
                bundle.lint.findings.len()
            );
            eprint!("{}", bundle.lint.text());
            findings += bundle.lint.findings.len();
        }
    }

    if findings > 0 {
        eprintln!("\nbundle lint reported {findings} finding(s)");
        std::process::exit(1);
    }
    Ok(())
}
