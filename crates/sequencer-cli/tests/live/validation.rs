//! Real-binary validation checks: argv in, exit code and message out. No hardware,
//! so these run in every `cargo test` — they are the end-to-end twin of the
//! in-process parser tests.

use crate::harness::{TempConfig, run};

fn template() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../example_profile.toml")
}

#[test]
fn the_shipped_template_passes_profile_check() {
    let config = TempConfig::new();
    let (status, out) = run(&config, &["profile-check", template()]);
    assert!(
        status.success(),
        "profile-check refused the template: {out}"
    );
}

#[test]
fn format_is_idempotent_and_keeps_comments() {
    let config = TempConfig::new();
    let copy = config.dir().join("fmt.toml");
    std::fs::copy(template(), &copy).expect("copy the template");
    let copy_arg = copy.to_str().expect("utf-8 temp path");

    let (status, out) = run(&config, &["profile-check", "--format", copy_arg]);
    assert!(status.success(), "first --format failed: {out}");
    let once = std::fs::read_to_string(&copy).expect("formatted file");

    let (status, out) = run(&config, &["profile-check", "--format", copy_arg]);
    assert!(status.success(), "second --format failed: {out}");
    let twice = std::fs::read_to_string(&copy).expect("formatted file");

    assert_eq!(once, twice, "--format must be idempotent");
    assert!(
        twice.contains("optional"),
        "the template's comments must survive formatting"
    );
}

#[test]
fn every_broken_profile_is_refused_with_its_reason() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "suppress = false is refused, never ignored",
            "suppress = false is not supported",
            "[defaults]\nsuppress = false\n[binds.f9]\nbind = \"pause\"\n",
        ),
        (
            "left/right-only trigger twins are one X grab",
            "are the same grab",
            "[binds.\"shift f9\"]\nbind = \"pause\"\n[binds.\"rshift f9\"]\nbind = \"pause\"\n",
        ),
        (
            "a bare key and a chord over it cannot coexist",
            "cannot both be triggers",
            "[binds.f9]\nbind = \"pause\"\n[binds.\"ctrl f9\"]\nbind = \"pause\"\n",
        ),
        (
            "emergency_stop may not double as a trigger",
            "is also the trigger",
            "[defaults]\nemergency_stop = \"f9\"\n[binds.f9]\nbind = \"pause\"\n",
        ),
        (
            "an unknown key is named, not guessed",
            "unknown key",
            "[binds.notakey]\nbind = \"pause\"\n",
        ),
    ];
    let config = TempConfig::new();
    for (what, fragment, text) in cases {
        let file = config.profile("broken.toml", text);
        let file_arg = file.to_str().expect("utf-8 temp path");
        let (status, out) = run(&config, &["profile-check", file_arg]);
        assert!(!status.success(), "{what}: was accepted");
        assert!(
            out.contains(fragment),
            "{what}: refusal must mention {fragment:?}, said: {out}"
        );
    }
}

#[test]
fn unapply_with_an_empty_set_says_so() {
    let config = TempConfig::new();
    let (status, out) = run(&config, &["profile-unapply"]);
    assert!(status.success(), "empty-set unapply failed: {out}");
    assert!(
        out.contains("nothing is applied."),
        "empty-set unapply said: {out}"
    );
}
