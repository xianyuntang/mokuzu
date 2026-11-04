//! Mokuzu's own manual, the README and docs/tools.md built into the binary, so the model can
//! answer how it works (cron jobs, memory, approval, setup) from the docs instead of guessing.

use serde_json::{Value, json};

use rig_core::completion::ToolDefinition;

const DOCS: [&str; 2] = [
    include_str!("../../../README.md"),
    include_str!("../../../docs/tools.md"),
];

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "manual".to_owned(),
        description: "Search your own user manual (mokuzu's README and tool docs): features, every built-in tool, cron jobs, \
                      webhooks, memory, skills, approval, Discord and voice setup, configuration, \
                      install. Call it before explaining how you or one of your features work, or \
                      how to set something up; never describe yourself from guesswork. Returns \
                      the sections matching the most words of `query`, or the list of sections."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "English keywords separated by spaces, e.g. `cron schedule`; the manual is in English.",
                },
            },
            "required": ["query"],
        }),
    }
}

pub fn search(args: &Value) -> String {
    let words: Vec<String> = args["query"]
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let sections = sections();
    let score = |section: &str| {
        let lower = section.to_lowercase();
        words.iter().filter(|w| lower.contains(w.as_str())).count()
    };
    let best = sections.iter().map(|s| score(s)).max().unwrap_or(0);
    if best == 0 {
        let headings: Vec<&str> = sections
            .iter()
            .filter_map(|s| s.lines().next())
            .filter(|line| line.starts_with('#'))
            .collect();
        return format!(
            "no section matches; search again with words from these headings:\n{}",
            headings.join("\n")
        );
    }
    sections
        .into_iter()
        .filter(|s| score(s) == best)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Each doc split before every `##` or `###` heading; a doc's intro comes first.
fn sections() -> Vec<&'static str> {
    DOCS.iter()
        .flat_map(|doc| {
            let starts: Vec<usize> = std::iter::once(0)
                .chain(doc.match_indices("\n##").map(|(i, _)| i + 1))
                .chain([doc.len()])
                .collect();
            starts
                .windows(2)
                .map(|w| doc[w[0]..w[1]].trim())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_section_with_the_most_words() {
        let hit = search(&json!({"query": "dave rejoins"}));
        assert!(hit.starts_with("### Voice calls"), "{hit}");
        assert!(!hit.contains("## Running in the background"));
    }

    #[test]
    fn lists_headings_when_nothing_matches() {
        let miss = search(&json!({"query": "zzqx"}));
        assert!(
            miss.contains("## Tools") && miss.contains("### `cron`"),
            "{miss}"
        );
    }
}
