//! The Python has no dedicated help test (`docs/GUIDE.md` is kept in sync by tests/test_cli.py);
//! these pin the same contract on the Rust side.

use super::*;

const VERSION: &str = "2.71.4";

#[test]
fn test_the_plain_guide_leads_with_the_underlined_version_heading() {
    let text = guide(false, VERSION);
    let heading = format!("Dunking Sheep {VERSION} guide");
    assert!(
        text.starts_with(&format!("{heading}\n{}\n", "=".repeat(heading.len()))),
        "{}",
        &text[..80]
    );
    assert!(!text.contains("@VERSION@") && !text.contains("@HEADING@"));
}

#[test]
fn test_the_underline_tracks_the_version_length() {
    for version in ["1.0", "2.71.4", "10.200.3000"] {
        let heading = format!("Dunking Sheep {version} guide");
        let text = guide(false, version);
        assert!(
            text.starts_with(&format!(
                "{heading}\n{}\n\nDunking Sheep sends",
                "=".repeat(heading.len())
            )),
            "{version}"
        );
    }
}

#[test]
fn test_the_markdown_guide_leads_with_a_hash_heading() {
    let text = guide(true, VERSION);
    assert!(text.starts_with(&format!("# Dunking Sheep {VERSION} guide\n\n")));
    assert!(!text.contains("@VERSION@"));
}

#[test]
fn test_the_version_is_the_only_difference_between_versions() {
    assert_eq!(
        guide(true, "9.9.9"),
        guide(true, VERSION).replace(VERSION, "9.9.9")
    );
}

#[test]
fn test_the_markdown_body_is_docs_guide_md() {
    // docs/GUIDE.md is the markdown rendering. Its heading carries whatever version was last
    // regenerated, so compare everything after the heading line.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/GUIDE.md");
    let docs = std::fs::read_to_string(&path).expect("docs/GUIDE.md");
    let body = |text: &str| {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .expect("a heading line")
    };
    assert_eq!(body(&guide(true, VERSION)), body(&docs));
}

#[test]
fn test_short_is_the_help_epilog_text() {
    assert!(SHORT
        .starts_with("Dunking Sheep types text (plus Enter) into terminal panes on a schedule."));
    assert!(SHORT.contains("quick start\n  dunkingsheep panes"));
    assert!(SHORT.ends_with("  dunkingsheep <command> -h   per-command flags and examples\n"));
}

#[test]
fn test_the_plain_and_markdown_guides_have_the_same_sections() {
    let plain = guide(false, VERSION);
    let markdown = guide(true, VERSION);
    for section in ["Troubleshooting", "Dunking Sheep"] {
        assert!(
            plain.contains(section) && markdown.contains(section),
            "{section}"
        );
    }
    assert!(markdown.contains("## Troubleshooting\n\n"));
    assert!(plain.contains("Troubleshooting\n---------------\n"));
}
