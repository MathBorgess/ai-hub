use std::fs;
use std::path::Path;

use crate::redact::redact_secrets;
use crate::{BriefPair, HandoffTurn, MemoryError};

// Last assistant excerpt in the brief is capped at 2_000 UTF-8 scalars to keep the brief compact.
const LAST_OUTPUT_BRIEF_CAP: usize = 2_000;

pub fn write_brief_pair(
    output_dir: &Path,
    session_index: u32,
    goal: &str,
    turn: &HandoffTurn,
) -> Result<BriefPair, MemoryError> {
    fs::create_dir_all(output_dir)?;

    let stem = format!("{:02}", session_index);
    let brief_path = output_dir.join(format!("{stem}.md"));
    let prompt_path = output_dir.join(format!("{stem}.prompt.md"));

    let last_excerpt = truncate_chars(&turn.last_output, LAST_OUTPUT_BRIEF_CAP);
    let last_section = if last_excerpt.is_empty() {
        "_Outgoing harness had no final assistant message in the local transcript._".to_string()
    } else {
        last_excerpt
    };

    let constraints = build_constraints(turn);
    let pointers = build_pointers(turn);

    let brief_body = format!(
        r#"# Handoff {stem}: harness switch brief

## Goal
{goal}

## In scope
- Continue the user's task in the incoming harness worktree
- Read only paths referenced in this brief or reachable from Pointers

## Out of scope
- Re-litigating decisions already captured under Constraints
- Modifying files outside the requested scope

## Constraints
{constraints}

## Done when
- [ ] Incoming harness reads this brief and executes the Goal
- [ ] Progress and result files updated per handoff skill

## Pointers
{pointers}

## Last turn (outgoing harness)
{last_section}

## Suggested skills
- handoff — brief format and dispatch rules

## Progress and result
Append one line per completed checklist item to the session progress file, then write the result file before exiting.
"#
    );

    let prompt_body = format!(
        r#"Read the session brief at `{brief}` and execute only that brief.
Append to the session progress file as you finish each item, and write the session result file before you exit.
Do not read other session briefs. Do not wait for the parent.
"#,
        brief = brief_path.display()
    );

    fs::write(&brief_path, redact_secrets(&brief_body))?;
    fs::write(&prompt_path, redact_secrets(&prompt_body))?;

    Ok(BriefPair {
        brief_path,
        prompt_path,
    })
}

fn build_constraints(turn: &HandoffTurn) -> String {
    let mut lines = Vec::new();
    if !turn.summary.is_empty() && turn.summary != "no last turn" {
        lines.push(format!("- Context: {}", turn.summary));
    }
    if turn.summary == "no last turn" {
        lines.push("- Outgoing harness transcript missing or had no assistant turn.".to_string());
    }
    for decision in &turn.decisions {
        lines.push(format!("- {decision}"));
    }
    if lines.is_empty() {
        "- (none recorded)".to_string()
    } else {
        lines.join("\n")
    }
}

fn build_pointers(turn: &HandoffTurn) -> String {
    let mut lines = Vec::new();
    for decision in &turn.decisions {
        if let Some(rest) = decision.strip_prefix("diff-stat:") {
            lines.push(format!("- worktree diff stat — `{rest}` (not pasted here)"));
        }
    }
    if lines.is_empty() {
        lines.push("- worktree diff stat — supplied by aihub-git at handoff time (path passed via `diff-stat:` decision)".to_string());
    }
    lines.join("\n")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

pub fn next_session_index(output_dir: &Path) -> u32 {
    let Ok(entries) = fs::read_dir(output_dir) else {
        return 1;
    };
    let mut max_idx = 0_u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(num) = name.strip_suffix(".md").and_then(|s| s.parse::<u32>().ok()) {
            max_idx = max_idx.max(num);
        }
    }
    max_idx + 1
}

/// Extracted context from a written brief file (`NN.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedBriefContext {
    pub goal: String,
    pub decisions: Vec<String>,
    pub last_output_summary: String,
}

/// Parses the goal, decisions, and last output summary from a written brief string.
pub fn parse_brief_context(content: &str) -> ExtractedBriefContext {
    let mut goal = String::new();
    let mut decisions = Vec::new();
    let mut last_output_summary = String::new();

    if let Some(pos) = content.find("## Goal\n") {
        let after = &content[pos + 8..];
        let end = after.find("\n## ").unwrap_or(after.len());
        goal = after[..end].trim().to_string();
    }

    if let Some(pos) = content.find("## Constraints\n") {
        let after = &content[pos + 15..];
        let end = after.find("\n## ").unwrap_or(after.len());
        let section = &after[..end];
        for line in section.lines() {
            let trimmed = line.trim();
            if let Some(bullet) = trimmed.strip_prefix("- ") {
                let bullet = bullet.trim();
                if bullet.starts_with("(none recorded)")
                    || bullet.starts_with("Outgoing harness transcript missing")
                {
                    continue;
                }
                if let Some(ctx) = bullet.strip_prefix("Context: ") {
                    if last_output_summary.is_empty() {
                        last_output_summary = ctx.trim().to_string();
                    }
                } else {
                    decisions.push(bullet.to_string());
                }
            }
        }
    }

    if let Some(pos) = content.find("## Last turn (outgoing harness)\n") {
        let after = &content[pos + 32..];
        let end = after.find("\n## ").unwrap_or(after.len());
        let turn_text = after[..end].trim();
        if !turn_text.is_empty()
            && !turn_text.starts_with("_Outgoing harness had no final assistant message")
        {
            last_output_summary = turn_text.to_string();
        }
    }

    ExtractedBriefContext {
        goal,
        decisions,
        last_output_summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_numbered_brief_pair_with_redaction() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-memory-brief-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let turn = HandoffTurn {
            summary: "continue bridge".into(),
            last_output:
                "Decision: ship it.\neyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxIn0.sig"
                    .to_string(),
            decisions: vec![
                "Decision: use JSONL".into(),
                "diff-stat: /tmp/stat.txt".into(),
            ],
        };

        let pair = write_brief_pair(&dir, 1, "Finish memory bridge", &turn).unwrap();
        assert!(pair.brief_path.ends_with("01.md"));
        assert!(pair.prompt_path.ends_with("01.prompt.md"));
        let brief = fs::read_to_string(pair.brief_path).unwrap();
        assert!(brief.contains("## Goal"));
        assert!(brief.contains("Finish memory bridge"));
        assert!(brief.contains("/tmp/stat.txt"));
        assert!(!brief.contains("eyJhbGci"));
        assert!(brief.contains("[REDACTED]"));

        let second = write_brief_pair(&dir, next_session_index(&dir), "Next", &turn).unwrap();
        assert!(second.brief_path.ends_with("02.md"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn brief_template_is_generic_without_repo_specific_instructions() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-memory-generic-brief-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let turn = HandoffTurn {
            summary: "refactor user auth".into(),
            last_output: "Done implementing user login.".to_string(),
            decisions: vec!["Decision: use Argon2".into()],
        };

        let pair = write_brief_pair(&dir, 1, "Implement user authentication", &turn).unwrap();
        let brief = fs::read_to_string(pair.brief_path).unwrap();

        // Must not contain repo-specific instructions or document references
        assert!(
            !brief.contains("CONTRACT.md"),
            "brief must not refer to CONTRACT.md"
        );
        assert!(
            !brief.contains("PLAN.md"),
            "brief must not refer to PLAN.md"
        );
        assert!(
            !brief.contains("sessions 05 and 08"),
            "brief must not mention sessions 05 and 08"
        );
        assert!(
            !brief.contains("memory bridge responsibilities"),
            "brief must not mention memory bridge"
        );

        // Must contain generic brief sections
        assert!(brief.contains("# Handoff 01: harness switch brief"));
        assert!(brief.contains("## Goal\nImplement user authentication"));
        assert!(brief.contains("## In scope"));
        assert!(brief.contains("## Out of scope"));
        assert!(brief.contains("## Constraints"));
        assert!(brief.contains("## Done when"));
        assert!(brief.contains("## Pointers"));
        assert!(brief.contains("## Last turn"));

        let _ = fs::remove_dir_all(&dir);
    }
}
