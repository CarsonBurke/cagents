use super::*;

pub(super) fn stream(agent: &str, follow: bool, format: &str, json_output: bool) -> Result<(), AppError> {
    let mut session = Session::new();
    let agent = session.agent(agent)?;
    let mut cursor = None;
    let mut first = true;
    let mut out = io::BufWriter::new(io::stdout().lock());
    loop {
        let Reply::Transcript(page) = session.call(Operation::Transcript { agent, cursor: cursor.clone() })? else {
            return Err(AppError::new("agent.transcript", "Unexpected transcript response"));
        };
        if json_output || format == "json" {
            if first || !page.parts.is_empty() || page.closed {
                let mut data = json!({"agent": agent, "parts": page.parts, "cursor": page.next,
                    "at_end": page.at_end, "closed": page.closed, "partial_history": page.partial_history});
                super::super::format::normalize(&mut data);
                writeln!(out, "{}", json!({"ok": true, "command": "agent.transcript", "data": data}))
                    .map_err(|e| AppError::new("output", e))?;
            }
        } else {
            if first && page.partial_history {
                writeln!(out, "> Older provider history is not present in this cached transcript.\n")
                    .map_err(|e| AppError::new("output", e))?;
            }
            for part in &page.parts {
                write!(out, "{}", markdown(part)).map_err(|e| AppError::new("output", e))?;
            }
        }
        out.flush().map_err(|e| AppError::new("output", e))?;
        first = false;
        if page.at_end && (!follow || page.closed) { break; }
        if !page.at_end && page.next == cursor {
            return Err(AppError::new("agent.transcript", "Transcript cursor did not advance"));
        }
        cursor = page.next;
        if page.at_end { thread::sleep(Duration::from_millis(200)); }
    }
    Ok(())
}

fn fenced(body: &str) -> String {
    let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or_default();
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}text\n{body}\n{fence}\n\n")
}

fn markdown(part: &crate::agent_execution::TranscriptPart) -> String {
    use crate::chat::{ItemContent, Role};
    let mut output = if part.offset == 0 {
        format!("## {}\n\n", match part.item.role { Role::User => "User", Role::Assistant => "Assistant", Role::System => "System" })
    } else { String::new() };
    match &part.item.content {
        ItemContent::Text { text, thinking, .. } => {
            if *thinking && part.offset == 0 { output.push_str("*Thinking*\n\n"); }
            output.push_str(text); if part.complete { output.push_str("\n\n"); }
        }
        ItemContent::Command { command, output: body, .. } => {
            if part.offset == 0 { output.push_str(&format!("Command: `{command}`\n\n")); }
            output.push_str(&fenced(body));
        }
        ItemContent::Tool { name, input, output: body, .. } => {
            if part.offset == 0 { output.push_str(&format!("Tool: {name}\n\n")); }
            if !input.is_empty() { output.push_str(&fenced(input)); }
            if !body.is_empty() { output.push_str(&fenced(body)); }
        }
        ItemContent::Changes { changes, .. } => for change in changes {
            output.push_str(&format!("File: {}\n\n", change.path)); output.push_str(&fenced(&change.diff));
        },
        ItemContent::WebSearch { query, results } => {
            output.push_str(&format!("Search: {query}\n\n"));
            for result in results { output.push_str(&format!("- [{}]({}): {}\n", result.title, result.url, result.snippet)); }
            output.push('\n');
        }
        ItemContent::Image { path, description } => output.push_str(&format!("![{description}]({path})\n\n")),
        ItemContent::Delivery { summary, body, .. } => { if part.offset == 0 { output.push_str(&format!("{summary}\n\n")); } output.push_str(body); if part.complete { output.push_str("\n\n"); } }
    }
    if part.attachment_omitted { output.push_str("> An oversized attachment path was omitted.\n\n"); }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transcript_arguments_and_markdown_preserve_user_and_assistant_text() {
        for args in [vec!["cagents", "agent", "transcript", "reviewer"], vec!["cagents", "agent", "transcript", "reviewer", "--follow", "--format", "json"]] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        let part = crate::agent_execution::TranscriptPart { item: crate::chat::Item {
            id: "message".into(), role: crate::chat::Role::Assistant, content: crate::chat::ItemContent::Text {
                text: "Readable transcript".into(), thinking: false, phase: crate::chat::Phase::Final }, at: None, started_at: None,
        }, offset: 0, complete: true, attachment_omitted: false };
        assert_eq!(markdown(&part), "## Assistant\n\nReadable transcript\n\n");
    }
}
