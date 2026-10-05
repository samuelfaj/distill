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

/// Below this many elements an array is not worth selecting from.
const JSON_MIN_ELEMENTS: usize = 8;

/// A JSON result whose largest array (the result itself, or one value of its
/// top-level object) is selected element by element; every other field is
/// kept.
pub(crate) struct JsonUnits {
    /// Each element, compact, one line.
    pub(crate) units: Vec<String>,
    /// Elements kept whatever the utility answers: focused ones, and inputs
    /// the next step may type into.
    pub(crate) required: Vec<bool>,
    /// Bytes of the result with the array emptied: always kept.
    pub(crate) envelope_bytes: usize,
    value: Value,
    key: Option<String>,
}

fn json_unit_required(element: &Value) -> bool {
    let Some(object) = element.as_object() else {
        return false;
    };
    let flag = |key: &str| object.get(key).and_then(Value::as_bool) == Some(true);
    flag("focused")
        || flag("focus")
        || object
            .get("role")
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|role| {
                matches!(
                    role.to_ascii_lowercase().as_str(),
                    "input" | "textbox" | "textarea" | "combobox" | "searchbox"
                )
            })
}

/// A JSON number literal reduced to sign, significant digits and exponent, so
/// `1.50` and `1.5` compare equal while `0.1000000000000000055511` and the
/// `0.1` it parses to do not.
fn canonical_number(literal: &str) -> Option<(bool, String, i64)> {
    let (negative, rest) = match literal.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, literal),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], rest[at + 1..].trim_start_matches('+').parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits: String = format!("{integer}{fraction}").trim_start_matches('0').to_owned();
    let mut exponent = exponent - fraction.len() as i64;
    while digits.ends_with('0') {
        digits.pop();
        exponent += 1;
    }
    if digits.is_empty() {
        return Some((false, String::new(), 0));
    }
    Some((negative, digits, exponent))
}

/// Whether every number in the JSON `text` survives a parse and re-serialise
/// with its value: an integer past 64 bits, or a decimal with more digits
/// than a double holds, would come back changed in a kept element.
fn numbers_round_trip(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    i += 1;
                }
                let literal = &text[start..i];
                let reserialised = serde_json::from_str::<Value>(literal).ok().map(|value| value.to_string());
                match reserialised {
                    Some(again) if canonical_number(&again) == canonical_number(literal) => {}
                    _ => return false,
                }
            }
            _ => i += 1,
        }
    }
    true
}

pub(crate) fn json_array_units(body: &str) -> Option<JsonUnits> {
    let mut value: Value = serde_json::from_str(body).ok()?;
    // Kept elements are re-serialised, so a number that would change is a
    // reason to keep line units.
    if !numbers_round_trip(body) {
        return None;
    }
    let key = match &value {
        Value::Array(_) => None,
        Value::Object(object) => Some(
            object
                .iter()
                .filter_map(|(key, value)| Some((key, value.as_array()?)))
                .filter(|(_, array)| array.len() >= JSON_MIN_ELEMENTS)
                .max_by_key(|(_, array)| serde_json::to_vec(array).map_or(0, |bytes| bytes.len()))?
                .0
                .clone(),
        ),
        _ => return None,
    };
    let array = match &key {
        None => value.as_array_mut()?,
        Some(key) => value.get_mut(key)?.as_array_mut()?,
    };
    if array.len() < JSON_MIN_ELEMENTS {
        return None;
    }
    let units = array
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let required = array.iter().map(json_unit_required).collect();
    let elements = std::mem::take(array);
    let envelope_bytes = value.to_string().len();
    match &key {
        None => value = Value::Array(elements),
        Some(key) => value[key.as_str()] = Value::Array(elements),
    }
    Some(JsonUnits {
        units,
        required,
        envelope_bytes,
        value,
        key,
    })
}

/// Valid compact JSON holding the envelope and the kept elements in order,
/// then the tool metadata and a footer with the omitted count and `pointer`.
pub(crate) fn rebuild_json_array(
    json: JsonUnits,
    kept: &BTreeSet<usize>,
    metadata: Option<&str>,
    pointer: &str,
) -> Option<String> {
    let JsonUnits {
        mut value, key, ..
    } = json;
    let array = match &key {
        None => value.as_array_mut()?,
        Some(key) => value.get_mut(key)?.as_array_mut()?,
    };
    let total = array.len();
    let mut index = 0usize;
    array.retain(|_| {
        index += 1;
        kept.contains(&(index - 1))
    });
    let kept_count = array.len();
    let mut output = serde_json::to_string(&value).ok()?;
    output.push('\n');
    if let Some(metadata) = metadata.filter(|m| !m.trim().is_empty()) {
        output.push_str("[tool metadata]\n");
        output.push_str(metadata.trim_end());
        output.push('\n');
    }
    let label = key.map_or_else(|| "array elements".to_owned(), |key| format!("`{key}` elements"));
    output.push_str(&format!(
        "[kept {kept_count} of {total} {label} by verified utility selection, {} omitted; {pointer}]",
        total - kept_count
    ));
    Some(output)
}

/// A line longer than this is cut into pieces of at most this many bytes, so a
/// selection can keep part of a minified line instead of all or none of it,
/// and the first and last two units a selection always keeps stay small on an
/// output of a few long lines.
pub(crate) const LONG_LINE_UNIT_BYTES: usize = 1_024;

/// `units` with every unit longer than `max` cut into pieces that concatenate
/// back to it byte for byte, each cut after a space, `,`, `;`, `>` or `}` in
/// the second half of the piece where there is one. `joins[i]` says piece `i`
/// continues on the same line as piece `i + 1`.
pub(crate) fn split_long_units(units: Vec<String>, max: usize) -> (Vec<String>, Vec<bool>) {
    let mut pieces = Vec::with_capacity(units.len());
    let mut joins = Vec::with_capacity(units.len());
    for unit in units {
        if unit.len() <= max || max < 8 {
            pieces.push(unit);
            joins.push(false);
            continue;
        }
        let mut start = 0;
        while unit.len() - start > max {
            let mut end = start + max;
            while !unit.is_char_boundary(end) {
                end -= 1;
            }
            let floor = start + max / 2;
            if let Some(cut) = unit.as_bytes()[floor..end]
                .iter()
                .rposition(|byte| matches!(byte, b' ' | b',' | b';' | b'>' | b'}'))
            {
                end = floor + cut + 1;
            }
            pieces.push(unit[start..end].to_owned());
            joins.push(true);
            start = end;
        }
        pieces.push(unit[start..].to_owned());
        joins.push(false);
    }
    (pieces, joins)
}

/// [`required_command_units`] over pieces: the first two and last two pieces,
/// and every piece of a line that carries evidence.
pub(crate) fn required_split_units(
    pieces: &[String],
    joins: &[bool],
    evidence: &HashSet<String>,
) -> Vec<bool> {
    let mut required = vec![false; pieces.len()];
    let mut start = 0;
    while start < pieces.len() {
        let mut end = start;
        while joins.get(end).copied().unwrap_or(false) && end + 1 < pieces.len() {
            end += 1;
        }
        let line: String = pieces[start..=end].concat();
        if evidence.iter().any(|item| line.contains(item)) {
            required[start..=end].iter_mut().for_each(|r| *r = true);
        }
        start = end + 1;
    }
    for (i, r) in required.iter_mut().enumerate() {
        *r |= i < 2 || i + 2 >= pieces.len();
    }
    required
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

/// [`plan_chunks`] that may leave a tail unselected: past `max_chunks`, the
/// first `max_chunks` chunks are returned with the rest as a second list, kept
/// verbatim by the caller. Only when that tail is under half the bytes: a
/// larger one could not let the result pass the 70% bar however well the head
/// is cut, so the plan defers as before.
pub(crate) fn plan_chunks_with_tail(
    units: &[String],
    cap: usize,
    max_chunks: usize,
) -> Result<(Vec<Range<usize>>, Vec<Range<usize>>), &'static str> {
    match plan_chunks(units, cap, max_chunks) {
        Err("defer:too-many-chunks") => {}
        planned => return planned.map(|chunks| (chunks, Vec::new())),
    }
    let mut chunks = plan_chunks(units, cap, usize::MAX)?;
    let tail = chunks.split_off(max_chunks);
    let bytes = |range: &Range<usize>| units[range.clone()].iter().map(String::len).sum::<usize>();
    let tail_bytes: usize = tail.iter().map(bytes).sum();
    let total: usize = units.iter().map(String::len).sum();
    if tail_bytes * 2 >= total {
        return Err("defer:too-many-chunks");
    }
    Ok((chunks, tail))
}

/// The unit bytes every selection keeps whatever the utility answers: the
/// required units, plus the tail [`plan_chunks_with_tail`] keeps whole. The
/// plan's own deferral when it does not fit.
pub(crate) fn kept_floor_bytes(
    units: &[String],
    required: &[bool],
    cap: usize,
    max_chunks: usize,
) -> Result<usize, &'static str> {
    let (_, tail) = plan_chunks_with_tail(units, cap, max_chunks)?;
    let in_tail = |index: usize| tail.iter().any(|range| range.contains(&index));
    Ok(units
        .iter()
        .enumerate()
        .filter(|(index, _)| required[*index] || in_tail(*index))
        .map(|(_, unit)| unit.len())
        .sum())
}

/// Chunk answers already paid for in this process: the same units, for the
/// same question, from the same model, get the same answer without a call.
/// Keys are hashes and names only, so no content is retained; failed chunks
/// are never remembered, and the oldest entry goes first past the bound.
const SELECTION_MEMO_ENTRIES: usize = 512;

type SelectionMemo = (
    std::collections::HashMap<String, ChunkAnswer>,
    std::collections::VecDeque<String>,
);

fn selection_memo() -> &'static std::sync::Mutex<SelectionMemo> {
    static MEMO: std::sync::OnceLock<std::sync::Mutex<SelectionMemo>> = std::sync::OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// The memo key for one chunk request.
pub(crate) fn selection_memo_key(
    endpoint: &str,
    model: &str,
    source_kind: &str,
    payload: &str,
    question: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let digest = |text: &str| format!("{:x}", Sha256::digest(text.as_bytes()));
    format!(
        "{endpoint}\n{model}\n{source_kind}\n{}\n{}",
        digest(payload),
        digest(question)
    )
}

pub(crate) fn selection_memo_get(key: &str) -> Option<ChunkAnswer> {
    selection_memo().lock().ok()?.0.get(key).cloned()
}

/// Remembers an answer; a failed chunk is not an answer and is not kept.
pub(crate) fn selection_memo_put(key: String, answer: &ChunkAnswer) {
    if *answer == ChunkAnswer::Failed {
        return;
    }
    let Ok(mut memo) = selection_memo().lock() else {
        return;
    };
    let (answers, order) = &mut *memo;
    if answers.insert(key.clone(), answer.clone()).is_none() {
        order.push_back(key);
    }
    while order.len() > SELECTION_MEMO_ENTRIES {
        if let Some(oldest) = order.pop_front() {
            answers.remove(&oldest);
        }
    }
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
    reconstruct_joined(units, &[], kept, kind, metadata, footer)
}

/// [`reconstruct`] over pieces from [`split_long_units`]: the kept pieces of
/// one line stay on one line, with the bytes omitted between them marked.
pub(crate) fn reconstruct_joined(
    units: &[String],
    joins: &[bool],
    kept: &BTreeSet<usize>,
    kind: UnitKind,
    metadata: Option<&str>,
    footer: String,
) -> String {
    let mut output = String::new();
    let mut omitted = 0;
    let mut start = 0;
    while start < units.len() {
        let mut end = start;
        while joins.get(end).copied().unwrap_or(false) && end + 1 < units.len() {
            end += 1;
        }
        let line = start..=end;
        start = end + 1;
        if !line.clone().any(|i| kept.contains(&i)) {
            omitted += 1;
            continue;
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
            omitted = 0;
        }
        let mut omitted_bytes = 0;
        for i in line {
            if kept.contains(&i) {
                if omitted_bytes > 0 {
                    output.push_str(&format!("[… {omitted_bytes} bytes omitted …]"));
                    omitted_bytes = 0;
                }
                output.push_str(&units[i]);
            } else {
                omitted_bytes += units[i].len();
            }
        }
        if omitted_bytes > 0 {
            output.push_str(&format!("[… {omitted_bytes} bytes omitted …]"));
        }
        output.push('\n');
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

/// File lines in order with their 1-based numbers; the first and last line
/// are always kept and each omitted run names its line range.
pub(crate) fn reconstruct_anchored_lines(lines: &[String], kept: &BTreeSet<usize>) -> String {
    let mut output = String::new();
    let mut omitted_start = None;
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if !kept.contains(&index) && index != 0 && index + 1 != lines.len() {
            omitted_start.get_or_insert(number);
            continue;
        }
        if let Some(start) = omitted_start.take() {
            output.push_str(&format!(
                "[… lines {start}-{prev} omitted; re-read with offset/limit …]\n",
                prev = number - 1
            ));
        }
        output.push_str(&format!("{number}→{line}\n"));
    }
    if let Some(start) = omitted_start {
        output.push_str(&format!(
            "[… lines {start}-{} omitted; re-read with offset/limit …]\n",
            lines.len()
        ));
    }
    output
}

/// Indices of a file's outline lines: Markdown headings outside code fences,
/// or the declaration lines `source_skeleton` keeps for source.
pub(crate) fn outline_lines(lines: &[String], markdown: bool) -> BTreeSet<usize> {
    let mut in_fence = false;
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            if !markdown {
                return distill_workspace::jev::crushers::is_signature_line(line);
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                return false;
            }
            let text = trimmed.trim_start_matches('#');
            !in_fence
                && (1..=6).contains(&(trimmed.len() - text.len()))
                && text.starts_with(char::is_whitespace)
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outline is what a thin read selection falls back to, so it must
    /// find the structure the model re-reads by: headings in prose (not a
    /// shell comment inside a fence) and declarations in source.
    #[test]
    fn outline_finds_headings_outside_fences_and_source_declarations() {
        let doc: Vec<String> = ["# Title", "text", "```sh", "# not a heading", "```", "## Usage", "#hashtag"]
            .map(str::to_owned)
            .into();
        assert_eq!(outline_lines(&doc, true).into_iter().collect::<Vec<_>>(), [0, 5]);
        let source: Vec<String> = ["use std::fmt;", "", "pub(crate) fn a() {", "    body();", "}"]
            .map(str::to_owned)
            .into();
        assert_eq!(outline_lines(&source, false).into_iter().collect::<Vec<_>>(), [0, 2]);
    }

    #[test]
    fn anchored_lines_render_exact_omission_ranges() {
        let lines: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
        let kept = [2, 24, 25].into_iter().collect();
        let rendered = reconstruct_anchored_lines(&lines, &kept);
        assert!(rendered.starts_with("1→line 1\n"));
        assert!(rendered.contains("[… lines 2-2 omitted; re-read with offset/limit …]"));
        assert!(rendered.contains("25→line 25\n26→line 26\n"));
        assert!(rendered.contains("[… lines 27-29 omitted; re-read with offset/limit …]"));
    }

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

#[cfg(test)]
mod json_and_long_line_tests {
    use super::*;

    fn snapshot() -> String {
        let mut elements: Vec<Value> = (0..20)
            .map(|i| json!({"name": format!("link {i}"), "ref": format!("page.{i}"), "role": "a", "value": ""}))
            .collect();
        elements[7] = json!({"name": "Search", "ref": "page.7", "role": "input", "value": ""});
        json!({"elements": elements, "title": "Inbox", "url": "https://example.com", "ok": true})
            .to_string()
    }

    /// A browser snapshot is one minified line: line units made it a single
    /// all-or-nothing unit. Elements are units, the envelope always stays, and
    /// the input the next step may type into is kept whatever the utility says.
    #[test]
    fn a_one_line_snapshot_is_selected_element_by_element() {
        let body = snapshot();
        assert_eq!(body.lines().count(), 1);
        let json = json_array_units(&body).expect("snapshot has an array");
        assert_eq!(json.units.len(), 20);
        assert!(json.units[3].contains(r#""ref":"page.3""#));
        assert_eq!(
            json.required.iter().enumerate().filter(|(_, r)| **r).map(|(i, _)| i).collect::<Vec<_>>(),
            [7],
            "only the input is forced, not the first and last elements"
        );
        let kept: BTreeSet<usize> = [3, 7].into_iter().collect();
        let rebuilt =
            rebuild_json_array(json, &kept, None, "full output stored at /tmp/full").unwrap();
        let (first, footer) = rebuilt.split_once('\n').unwrap();
        let value: Value = serde_json::from_str(first).expect("the excerpt is valid JSON");
        assert_eq!(value["title"], "Inbox", "envelope fields survive");
        assert_eq!(value["url"], "https://example.com");
        let refs: Vec<&str> = value["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["ref"].as_str().unwrap())
            .collect();
        assert_eq!(refs, ["page.3", "page.7"], "kept elements keep their order and refs");
        assert!(footer.contains("kept 2 of 20 `elements` elements"), "{footer}");
        assert!(footer.contains("18 omitted"), "{footer}");
        assert!(footer.contains("full output stored at /tmp/full"), "{footer}");
    }

    #[test]
    fn a_top_level_array_is_selected_and_rebuilt_as_an_array() {
        let body = Value::from((0..12).map(|i| json!({"title": format!("w{i}")})).collect::<Vec<_>>())
            .to_string();
        let json = json_array_units(&body).unwrap();
        let rebuilt = rebuild_json_array(json, &[0].into_iter().collect(), Some("exit=0"), "p").unwrap();
        let value: Value = serde_json::from_str(rebuilt.lines().next().unwrap()).unwrap();
        assert_eq!(value, json!([{"title": "w0"}]));
        assert!(rebuilt.contains("[tool metadata]\nexit=0\n"));
        assert!(rebuilt.contains("kept 1 of 12 array elements"));
    }

    /// Anything that is not JSON with a large array keeps today's line units.
    #[test]
    fn non_json_or_small_arrays_keep_line_units() {
        assert!(json_array_units("not json at all").is_none());
        assert!(json_array_units(r#"{"elements":[1,2,3],"ok":true}"#).is_none());
        assert!(json_array_units(r#""just a string""#).is_none());
        assert!(json_array_units(r#"{"elements":[1,2,3,4,5,6,7,8"#).is_none(), "a cut body");
    }

    /// Kept elements must carry the values the tool returned: an id past 64
    /// bits or an over-precise amount would come back changed, with no
    /// omission marker, so that JSON keeps line units. Numbers that only
    /// change spelling (`1.50`, `1e5`) do not block the JSON path.
    #[test]
    fn json_whose_numbers_would_change_keeps_line_units() {
        let items = |value: &str| {
            let elements: Vec<String> =
                (0..10).map(|i| format!(r#"{{"id":{i},"v":{value},"s":"-1e999 inside a string"}}"#)).collect();
            format!(r#"{{"items":[{}]}}"#, elements.join(","))
        };
        assert!(json_array_units(&items("123456789012345678901234")).is_none());
        assert!(json_array_units(&items("0.1000000000000000055511")).is_none());
        assert!(json_array_units(&items("18446744073709551615")).is_some());
        assert!(json_array_units(&items("-12.50")).is_some());
        assert!(json_array_units(&items("1e5")).is_some());
    }

    /// Pieces are byte-exact: kept together they are the line again, so a
    /// piece copied back is a real substring of the output.
    #[test]
    fn long_lines_split_into_byte_exact_pieces() {
        let long = format!("{}é{}", "key=value, ".repeat(400), "tail".repeat(300));
        let (pieces, joins) =
            split_long_units(vec!["short".into(), long.clone(), "end".into()], LONG_LINE_UNIT_BYTES);
        assert_eq!(pieces[0], "short");
        assert_eq!(pieces.last().unwrap(), "end");
        assert!(pieces.len() > 4);
        assert!(pieces.iter().all(|piece| piece.len() <= LONG_LINE_UNIT_BYTES));
        assert_eq!(pieces[1..pieces.len() - 1].concat(), long);
        assert_eq!(joins.iter().filter(|j| **j).count(), pieces.len() - 3);
        assert!(pieces[1].ends_with(", ") || pieces[1].ends_with(','), "{}", &pieces[1][pieces[1].len() - 4..]);
    }

    /// A single long line was all required (first and last two units); its
    /// pieces are not, but a line carrying evidence keeps every piece.
    #[test]
    fn evidence_keeps_every_piece_of_its_line_and_position_counts_pieces() {
        let long = format!("{}error: boom{}", "a ".repeat(1500), " b".repeat(1500));
        let lines = vec!["first".to_owned(), "x ".repeat(3000), long, "last".to_owned()];
        let (pieces, joins) = split_long_units(lines, LONG_LINE_UNIT_BYTES);
        let evidence: HashSet<String> = ["error: boom".to_owned()].into_iter().collect();
        let required = required_split_units(&pieces, &joins, &evidence);
        let error_start = pieces.iter().position(|p| p.starts_with("a a")).unwrap();
        let mut error_end = error_start;
        while joins[error_end] {
            error_end += 1;
        }
        assert!(error_end > error_start, "the error line was split");
        assert!((error_start..=error_end).all(|i| required[i]), "every piece of the error line stays");
        assert!(required[0] && required[1] && required[pieces.len() - 1]);
        assert!(!required[2], "a middle piece of an ordinary line is selectable");
        let no_evidence = required_split_units(&pieces, &joins, &HashSet::new());
        assert!(no_evidence.iter().filter(|r| **r).count() <= 4);
    }

    #[test]
    fn kept_pieces_stay_on_their_line_with_the_gap_marked() {
        let pieces: Vec<String> = ["aaaa", "bbbb", "cccc", "next line"].map(str::to_owned).into();
        let joins = [true, true, false, false];
        let text = reconstruct_joined(
            &pieces,
            &joins,
            &[0, 2].into_iter().collect(),
            UnitKind::Lines,
            None,
            "[footer]".into(),
        );
        assert_eq!(text, "aaaa[… 4 bytes omitted …]cccc\n[… 1 lines omitted …]\n[footer]");
    }

    /// Past the chunk limit a small tail is kept whole instead of the whole
    /// result going raw; a tail too large to ever pass the 70% bar defers as
    /// before, so no call is paid for nothing.
    #[test]
    fn too_many_chunks_select_the_head_and_keep_a_small_tail() {
        let units: Vec<String> = (0..10).map(|i| format!("{i:03} {}", "x".repeat(96))).collect();
        assert_eq!(plan_chunks(&units, 120, 8), Err("defer:too-many-chunks"));
        let (head, tail) = plan_chunks_with_tail(&units, 120, 8).expect("a two-unit tail is small");
        assert_eq!((head.len(), tail.len()), (8, 2));
        assert_eq!(tail[0].start, 8);
        let answers: Vec<ChunkAnswer> = (0..8)
            .map(|_| ChunkAnswer::Nothing)
            .chain(tail.iter().map(|_| ChunkAnswer::Failed))
            .collect();
        let chunks: Vec<_> = head.into_iter().chain(tail).collect();
        let kept = merge(&chunks, &answers, &[false; 10]).expect("the head answered");
        assert_eq!(kept.into_iter().collect::<Vec<_>>(), [8, 9], "the tail stays verbatim");

        let many: Vec<String> = (0..20).map(|i| format!("{i:03} {}", "x".repeat(96))).collect();
        assert_eq!(plan_chunks_with_tail(&many, 120, 8), Err("defer:too-many-chunks"));
        let (fits, none) = plan_chunks_with_tail(&units, 4_096, 8).unwrap();
        assert_eq!((fits.len(), none.len()), (1, 0));
    }

    /// The memo returns an answer already paid for and never remembers a
    /// failure, so a chunk that failed is asked again next time.
    #[test]
    fn the_selection_memo_keeps_answers_not_failures() {
        let key = |question: &str| selection_memo_key("http://memo-test", "m", "shell", "[U1] a", question);
        assert_ne!(key("q1"), key("q2"), "the question is part of the key");
        selection_memo_put(key("q1"), &ChunkAnswer::Ids(vec![1]));
        selection_memo_put(key("q2"), &ChunkAnswer::Failed);
        assert_eq!(selection_memo_get(&key("q1")), Some(ChunkAnswer::Ids(vec![1])));
        assert_eq!(selection_memo_get(&key("q2")), None);
        assert!(!key("q1").contains("[U1] a"), "no content is retained in the key");
    }
}
