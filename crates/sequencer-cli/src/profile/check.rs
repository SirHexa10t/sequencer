//! `profile-check`: say whether a binds file is sound, and optionally tidy it.
//!
//! Checking is the same [`super::format::parse`] every run performs — one validator, so
//! a file this command blesses is a file `profile-apply` will accept. What it adds is
//! *not applying*: no symlink, no lock, no grabs, so it is safe to wire into an editor
//! or a pre-commit hook.
//!
//! `--format` rewrites the file through `toml_edit`, which preserves every comment and
//! only touches what the style rules name. Formatting is refused for a file that does
//! not parse: reformatting something you have not understood is how comments end up
//! attached to the wrong step.

use std::path::Path;

use sequencer_core::input::Key;

use crate::args::ProfileCheckArgs;
use crate::{Deps, Error, Result, exit};

use super::format::{is_modifier, parse};

/// `sequencer profile-check`.
///
/// # Errors
///
/// If a file cannot be read or written. A file that fails *validation* is reported and
/// counted, not returned as an error — checking several files should report on all of
/// them, and the exit code carries the verdict.
pub(crate) fn profile_check(args: &ProfileCheckArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let mut bad = 0_u32;
    for file in &args.files {
        let path = file.display().to_string();
        let text = std::fs::read_to_string(file).map_err(|source| Error::ScriptRead {
            path: path.clone(),
            source,
        })?;
        match parse(&text) {
            Err(detail) => {
                writeln!(deps.out, "{path}: {detail}")?;
                bad += 1;
            }
            Ok(profile) => {
                if args.format {
                    match reformat(&text) {
                        Some(tidied) if tidied != text => {
                            write_back(file, &tidied)?;
                            writeln!(deps.out, "{path}: ok, reformatted")?;
                        }
                        _ => writeln!(deps.out, "{path}: ok, already tidy")?,
                    }
                } else {
                    writeln!(
                        deps.out,
                        "{path}: ok ({} binds{})",
                        profile.binds.len(),
                        profile
                            .program
                            .as_ref()
                            .map_or(String::new(), |p| format!(", for program {p}"))
                    )?;
                }
            }
        }
    }
    Ok(if bad == 0 { exit::OK } else { exit::FAILURE })
}

/// Writes `text` over `path` through a temporary file in the same directory.
///
/// Rename is atomic within a filesystem, so an interrupted format leaves the original
/// intact rather than a half-written profile the manager might load.
fn write_back(path: &Path, text: &str) -> Result<()> {
    let temporary = path.with_extension("toml.tmp");
    let fail = |source| Error::ScriptRead {
        path: temporary.display().to_string(),
        source,
    };
    std::fs::write(&temporary, text).map_err(fail)?;
    std::fs::rename(&temporary, path).map_err(fail)
}

/// Applies the style rules, keeping every comment. `None` if the text stops parsing —
/// the caller then leaves the file alone.
fn reformat(text: &str) -> Option<String> {
    let mut document = text.parse::<toml_edit::DocumentMut>().ok()?;
    let binds = document.get_mut("binds")?.as_table_mut()?;
    for (_, bind) in binds.iter_mut() {
        let Some(table) = bind.as_table_like_mut() else {
            continue;
        };
        if let Some(seq) = table.get_mut("seq")
            && let Some(array) = seq.as_array_mut()
        {
            tidy_steps(array);
        }
    }
    Some(document.to_string())
}

/// Rewrites one `seq` array: modifiers first in each chord, operands column-aligned,
/// and one indent level inside every RNG block.
fn tidy_steps(array: &mut toml_edit::Array) {
    let steps: Vec<String> = array
        .iter()
        .filter_map(|value| value.as_str().map(canonical_step))
        .collect();
    if steps.len() != array.len() {
        return; // A non-string entry means this is not ours to tidy.
    }
    let width = steps
        .iter()
        .filter_map(|step| keyword_of(step).map(str::len))
        .max()
        .unwrap_or(0);

    let mut depth = 0_usize;
    let rewritten: Vec<(String, String)> = steps
        .iter()
        .map(|step| {
            if is_block_end(step) {
                depth = depth.saturating_sub(1);
            }
            let prefix = format!("\n    {}", "    ".repeat(depth));
            if is_block_start(step) {
                depth += 1;
            }
            (prefix, align(step, width))
        })
        .collect();

    for (index, value) in array.iter_mut().enumerate() {
        let (prefix, text) = &rewritten[index];
        // Comments live in the element's own prefix decor. Carry them across onto the
        // replacement value — the decor belongs to the value, so it has to be set
        // *after* the swap or the new value arrives bare.
        let carried: Vec<String> = value
            .decor()
            .prefix()
            .and_then(toml_edit::RawString::as_str)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        let mut new_prefix = String::new();
        for comment in carried {
            new_prefix.push_str(prefix);
            new_prefix.push_str(&comment);
        }
        new_prefix.push_str(prefix);

        let mut replacement = toml_edit::Value::from(text.clone());
        replacement.decor_mut().set_prefix(new_prefix);
        *value = replacement;
    }
    array.set_trailing("\n");
    array.set_trailing_comma(true);
}

/// The step's leading keyword, if it has one (`PRESS`, `RELEASE`, `WAIT`, `RNG`).
fn keyword_of(step: &str) -> Option<&str> {
    let word = step.split_whitespace().next()?;
    matches!(word, "PRESS" | "RELEASE" | "WAIT" | "RNG").then_some(word)
}

fn is_block_start(step: &str) -> bool {
    keyword_of(step) == Some("RNG")
}

fn is_block_end(step: &str) -> bool {
    step.eq_ignore_ascii_case("GNR")
}

/// Uppercases the keyword and puts a chord's modifiers first — `d ctrl` reads as
/// `ctrl d` afterwards, which is how everyone writes chords by hand anyway.
fn canonical_step(step: &str) -> String {
    let mut tokens = step.split_whitespace();
    let Some(first) = tokens.next() else {
        return String::new();
    };
    let upper = first.to_ascii_uppercase();
    let (keyword, rest): (Option<String>, Vec<&str>) =
        if matches!(upper.as_str(), "PRESS" | "RELEASE" | "WAIT" | "RNG") {
            (Some(upper), tokens.collect())
        } else if first.eq_ignore_ascii_case("GNR") {
            return "GNR".to_owned();
        } else {
            (None, std::iter::once(first).chain(tokens).collect())
        };

    // WAIT's operand is a duration, not keys; leave it exactly as written.
    let operands = if keyword.as_deref() == Some("WAIT") || keyword.as_deref() == Some("RNG") {
        rest.join(" ")
    } else {
        sort_modifiers_first(&rest)
    };
    match keyword {
        Some(word) if operands.is_empty() => word,
        Some(word) => format!("{word} {operands}"),
        None => operands,
    }
}

/// Stable partition: every modifier, in the order written, then everything else.
fn sort_modifiers_first(tokens: &[&str]) -> String {
    fn is_mod(token: &str) -> bool {
        token.parse::<Key>().is_ok_and(is_modifier)
    }
    let mods = tokens.iter().filter(|token| is_mod(token));
    let rest = tokens.iter().filter(|token| !is_mod(token));
    mods.chain(rest).copied().collect::<Vec<_>>().join(" ")
}

/// Pads a keyword so operands across the block start in the same column.
fn align(step: &str, width: usize) -> String {
    let Some(keyword) = keyword_of(step) else {
        return step.to_owned();
    };
    let rest = step[keyword.len()..].trim_start();
    if rest.is_empty() {
        return step.to_owned();
    }
    format!("{keyword:<width$} {rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three style rules, on one array: keyword case, modifiers first, alignment,
    /// and an indent level inside the RNG block.
    #[test]
    fn formatting_applies_the_style_rules() {
        let text = "[binds.F5]\nseq = [\"space\", \"press q ctrl\", \"rng 50%\", \
                    \"wait 15ms\", \"gnr\"]\n";
        let tidied = reformat(text).expect("parses");
        assert!(tidied.contains("\"PRESS ctrl q\""), "{tidied}");
        assert!(tidied.contains("\"RNG   50%\""), "aligned: {tidied}");
        assert!(
            tidied.contains("\n        \"WAIT  15ms\""),
            "indented: {tidied}"
        );
        assert!(tidied.contains("\n    \"GNR\""), "dedented: {tidied}");
    }

    /// Nested RNG blocks each earn their own indent level, and every GNR dedents the
    /// step that follows it — the shape a reader scans to see what a roll covers.
    #[test]
    fn nested_blocks_indent_by_depth() {
        let text = "[binds.F5]\nseq = [\"rng 0.5\", \"press ctrl q\", \"rng 0.9\", \
                    \"wait 15ms\", \"gnr\", \"release ctrl q\", \"gnr\"]\n";
        let tidied = reformat(text).expect("parses");
        for (step, indent) in [
            ("\"RNG     0.5\"", 4),
            ("\"PRESS   ctrl q\"", 8),
            ("\"RNG     0.9\"", 8),
            ("\"WAIT    15ms\"", 12),
            ("\"RELEASE ctrl q\"", 8),
        ] {
            let line = tidied
                .lines()
                .find(|line| line.trim() == format!("{step},"))
                .unwrap_or_else(|| panic!("{step} missing from:\n{tidied}"));
            assert_eq!(
                line.len() - line.trim_start().len(),
                indent,
                "{step} sits at the wrong depth in:\n{tidied}"
            );
        }
        // Both GNRs close a level: the inner one back to 8, the outer back to 4.
        let closes: Vec<usize> = tidied
            .lines()
            .filter(|line| line.trim() == "\"GNR\",")
            .map(|line| line.len() - line.trim_start().len())
            .collect();
        assert_eq!(closes, vec![8, 4], "in:\n{tidied}");
    }

    /// Comments are the whole reason this uses a format-preserving parser.
    #[test]
    fn formatting_keeps_comments() {
        let text = "[binds.F5]\nseq = [\n    # jump\n    \"space\",\n]\n";
        let tidied = reformat(text).expect("parses");
        assert!(tidied.contains("# jump"), "{tidied}");
        assert!(tidied.contains("\"space\""), "{tidied}");
    }

    /// Formatting is idempotent — a tidy file is left byte-identical, which is what
    /// makes it safe in a pre-commit hook.
    #[test]
    fn formatting_twice_changes_nothing_the_second_time() {
        let text = "[binds.F5]\nseq = [\"space\", \"press ctrl q\", \"rng 1/2\", \"a\", \"gnr\"]\n";
        let once = reformat(text).expect("parses");
        let twice = reformat(&once).expect("still parses");
        assert_eq!(once, twice);
    }

    /// A file that does not parse as TOML is left alone rather than half-rewritten.
    #[test]
    fn broken_toml_is_not_reformatted() {
        assert!(reformat("[binds.F5\nseq = [").is_none());
    }
}
