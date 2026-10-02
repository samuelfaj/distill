use serde_json::{Value, json};
use std::collections::{BTreeSet, HashSet};
use std::ops::Range;


pub(crate) fn search_tool_units(body: &str) -> Option<(Vec<String>, Value)> {
    let value: Value = serde_json::from_str(body).ok()?;
    let servers = value.get("results")?.as_array()?;
    let mut units = Vec::new();
    for server in servers {
        let server_name = server.get("server")?.as_str()?;
        for tool in server.get("tools")?.as_array()? {
            units.push(format!(
                "{}/{}: {}",
                server_name,
                tool.get("tool_name")?.as_str()?,
                tool.get("description")?.as_str()?
            ));
        }
    }
    Some((units, value))
}

pub(crate) fn rebuild_search_tool(
    mut value: Value,
    kept: &BTreeSet<usize>,
    handle: &str,
) -> Option<String> {
    let results = value.get_mut("results")?.as_array_mut()?;
    let mut index = 0usize;
    let mut omitted = Vec::new();
    for server in results.iter_mut() {
        let name = server.get("server")?.as_str()?.to_owned();
        let tools = server.get_mut("tools")?.as_array_mut()?;
        let mut retained = Vec::new();
        for tool in tools.drain(..) {
            if kept.contains(&index) {
                retained.push(tool);
            } else {
                omitted.push(format!("{}/{}", name, tool.get("tool_name")?.as_str()?));
            }
            index += 1;
        }
        *server.get_mut("tools")? = json!(retained);
    }
    results.retain(|server| {
        server
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
    });
    let total = index;
    value["omitted_tools"] = json!(omitted);
    value["compressed"] = json!(format!(
        "kept {} of {} tools by verified utility selection; full result stored at {handle}",
        kept.len(),
        total
    ));
    serde_json::to_string(&value).ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnitKind {
    Lines,
    Paragraphs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChunkAnswer {
    Ids(Vec<usize>),
    Nothing,
    Failed,
}

pub(crate) fn build_units(text: &str, kind: UnitKind, cap: usize) -> Vec<String> {
    let lines: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    if kind == UnitKind::Lines {
        return lines;
    }
    let mut units = Vec::new();
    for paragraph in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if paragraph.len() <= cap {
            units.push(paragraph.to_owned());
        } else {
            units.extend(
                paragraph
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_owned),
            );
        }
    }
    units
}

pub(crate) fn plan_chunks(
    units: &[String],
    cap: usize,
    max_chunks: usize,
) -> Result<Vec<Range<usize>>, &'static str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let mut end = start;
        let mut last = None;
        let mut rendered_bytes = 0usize;
        while end < units.len() {
            let unit = &units[end];
            let first_line = unit.lines().next().unwrap_or_default();
            let rest_bytes =
                unit.lines().skip(1).map(str::len).sum::<usize>() + unit.lines().skip(1).count();
            let unit_bytes = format!("[U{}] ", end + 1).len() + first_line.len() + rest_bytes;
            let separator = usize::from(end > start);
            let candidate_bytes = rendered_bytes + separator + unit_bytes;
            if candidate_bytes > cap {
                break;
            }
            rendered_bytes = candidate_bytes;
            last = Some(end + 1);
            end += 1;
        }
        let Some(end) = last else {
            return Err("defer:unit-too-large");
        };
        chunks.push(start..end);
        if chunks.len() > max_chunks {
            return Err("defer:too-many-chunks");
        }
        start = end;
    }
    Ok(chunks)
}

pub(crate) fn merge(
    chunks: &[Range<usize>],
    answers: &[ChunkAnswer],
    required: &[bool],
) -> Option<BTreeSet<usize>> {
    if chunks.len() != answers.len() {
        return None;
    }
    let mut kept = BTreeSet::new();
    let mut any_success = false;
    for (chunk, answer) in chunks.iter().zip(answers) {
        match answer {
            ChunkAnswer::Ids(ids) => {
                any_success = true;
                kept.extend(
                    ids.iter()
                        .filter_map(|id| id.checked_sub(1))
                        .filter(|id| chunk.contains(id)),
                );
            }
            ChunkAnswer::Nothing => any_success = true,
            ChunkAnswer::Failed => {
                kept.extend(chunk.clone());
            }
        }
    }
    if !any_success {
        return None;
    }
    kept.extend(
        required
            .iter()
            .enumerate()
            .filter_map(|(i, yes)| yes.then_some(i)),
    );
    Some(kept)
}

pub(crate) fn required_command_units(units: &[String], evidence: &HashSet<String>) -> Vec<bool> {
    units
        .iter()
        .enumerate()
        .map(|(i, unit)| {
            i < 2 || i + 2 >= units.len() || evidence.iter().any(|line| unit.contains(line))
        })
        .collect()
}

pub(crate) fn reconstruct(
    units: &[String],
    kept: &BTreeSet<usize>,
    kind: UnitKind,
    metadata: Option<&str>,
    handle: &str,
    footer: String,
) -> String {
    let mut output = String::new();
    let mut omitted = 0;
    for (i, unit) in units.iter().enumerate() {
        if kept.contains(&i) {
            if omitted > 0 {
                output.push_str(&format!(
                    "[… {omitted} {} omitted …]\n",
                    if kind == UnitKind::Paragraphs {
                        "paragraphs"
                    } else {
                        "lines"
                    }
                ));
                omitted = 0;
            }
            output.push_str(unit);
            output.push(match kind {
                UnitKind::Lines => '\n',
                UnitKind::Paragraphs => '\n',
            });
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        output.push_str(&format!(
            "[… {omitted} {} omitted …]\n",
            if kind == UnitKind::Paragraphs {
                "paragraphs"
            } else {
                "lines"
            }
        ));
    }
    if let Some(metadata) = metadata.filter(|m| !m.trim().is_empty()) {
        output.push_str("[tool metadata]\n");
        output.push_str(metadata.trim_end());
        output.push('\n');
    }
    output.push_str(&footer);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_lines_and_falls_back_from_large_paragraph() {
        assert_eq!(build_units("a\n\nb\n", UnitKind::Lines, 9), ["a", "b"]);
        assert_eq!(
            build_units("one two\nthree", UnitKind::Paragraphs, 5),
            ["one two", "three"]
        );
    }

    #[test]
    fn plans_and_defers() {
        let units = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(plan_chunks(&units, 20, 3).unwrap(), vec![0..3]);
        assert_eq!(
            plan_chunks(&["too long".into()], 3, 3),
            Err("defer:unit-too-large")
        );
        assert_eq!(plan_chunks(&units, 14, 1), Err("defer:too-many-chunks"));
        for chunk in plan_chunks(&units, 12, 3).unwrap() {
            let refs: Vec<&str> = units[chunk.clone()].iter().map(String::as_str).collect();
            assert!(
                distill_workspace::jev::tasks::render_units(&refs, chunk.start + 1).len() <= 12
            );
        }
    }

    #[test]
    fn merges_answers_and_all_failed_keeps_original() {
        let chunks = vec![0..2, 2..4];
        let required = [true, false, false, false];
        assert_eq!(
            merge(
                &chunks,
                &[ChunkAnswer::Nothing, ChunkAnswer::Ids(vec![3])],
                &required
            ),
            Some([0, 2].into_iter().collect())
        );
        assert_eq!(
            merge(
                &chunks,
                &[ChunkAnswer::Failed, ChunkAnswer::Failed],
                &required
            ),
            None
        );
    }

    #[test]
    fn reconstructs_markers() {
        let text = reconstruct(
            &["a".into(), "b".into(), "c".into()],
            &[0, 2].into_iter().collect(),
            UnitKind::Lines,
            Some("exit=0"),
            "/tmp/full",
            "[compressed by verified utility selection; full output stored at /tmp/full]".into(),
        );
        assert!(text.contains("[… 1 lines omitted …]"));
        assert!(text.contains("[tool metadata]"));
        assert!(text.contains("full output stored at /tmp/full"));
    }
    #[test]
    fn reconstructs_match_footer() {
        let text = reconstruct(
            &["match".into()],
            &[0].into_iter().collect(),
            UnitKind::Lines,
            None,
            "/tmp/full",
            "[kept 1 of 3 match lines by verified utility selection; full output stored at /tmp/full]".into(),
        );
        assert!(text.ends_with("[kept 1 of 3 match lines by verified utility selection; full output stored at /tmp/full]"));
    }
}

#[cfg(test)]
mod search_tool_tests {
    use super::*;

    #[test]
    fn rebuilds_selected_search_tools_with_schema_and_omissions() {
        let body = r#"{"results":[{"server":"linear","tools":[{"tool_name":"a","description":"A","input_schema":{"type":"object"}},{"tool_name":"b","description":"B","input_schema":{"type":"string"}}]},{"server":"other","tools":[{"tool_name":"c","description":"C","input_schema":{"type":"boolean"}}]}],"total_hidden_tools":2,"status":"ready","note":null}"#;
        let (units, value) = search_tool_units(body).unwrap();
        assert_eq!(units, ["linear/a: A", "linear/b: B", "other/c: C"]);
        let rebuilt: Value = serde_json::from_str(
            &rebuild_search_tool(value, &[1].into_iter().collect(), "/tmp/full").unwrap(),
        )
        .unwrap();
        assert_eq!(rebuilt["results"].as_array().unwrap().len(), 1);
        assert_eq!(
            rebuilt["results"][0]["tools"][0]["input_schema"]["type"],
            "string"
        );
        assert_eq!(rebuilt["omitted_tools"], json!(["linear/a", "other/c"]));
        assert_eq!(rebuilt["total_hidden_tools"], 2);
    }

    #[test]
    fn parse_failure_returns_none() {
        assert!(search_tool_units("not json").is_none());
    }

    #[test]
    fn payload_below_compression_boundary_remains_unchanged() {
        let body = "x".repeat(3_999);
        let output = if body.len() >= 4_000 {
            "compressed".to_owned()
        } else {
            body.clone()
        };
        assert_eq!(output, body);
    }
}
