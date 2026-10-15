use super::*;

pub(super) fn stream(agent: &str, follow: bool, format: &str, json_output: bool) -> Result<(), AppError> {
    let mut session = Session::new();
    let agent = session.agent(agent)?;
    let mut cursor = None;
    let mut first = true;
    let mut markdown_output=Markdown::default();
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
                write!(out, "{}", markdown_output.part(part)).map_err(|e| AppError::new("output", e))?;
            }
        }
        let finished=page.at_end && (!follow || page.closed);
        if finished && !(json_output || format=="json") {
            write!(out,"{}",markdown_output.finish()).map_err(|e|AppError::new("output",e))?;
        }
        out.flush().map_err(|e| AppError::new("output", e))?;
        first = false;
        if finished { break; }
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

#[derive(Default)]
struct Markdown { open_prose:Option<String> }
impl Markdown {
    fn part(&mut self,part:&crate::agent_execution::TranscriptPart)->String {
        let prose=matches!(&part.item.content,crate::chat::ItemContent::Text{..}|crate::chat::ItemContent::Delivery{..});
        let continuation=prose && self.open_prose.as_deref()==Some(part.item.id.as_str());
        let mut output=if self.open_prose.is_some() && !continuation {self.finish()} else {String::new()};
        output.push_str(&markdown(part,!continuation));
        if prose {self.open_prose=Some(part.item.id.clone());}
        output
    }
    fn finish(&mut self)->&'static str {if self.open_prose.take().is_some(){"\n\n"}else{""}}
}
fn markdown(part: &crate::agent_execution::TranscriptPart,heading:bool) -> String {
    use crate::chat::{ItemContent, Role};
    let mut output = if heading && part.offset == 0 {
        format!("## {}\n\n", match part.item.role { Role::User => "User", Role::Assistant => "Assistant", Role::System => "System" })
    } else { String::new() };
    match &part.item.content {
        ItemContent::Text { text, thinking, .. } => {
            if *thinking && heading && part.offset == 0 { output.push_str("*Thinking*\n\n"); }
            output.push_str(text);
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
        ItemContent::Delivery { summary, body, .. } => { if heading && part.offset == 0 { output.push_str(&format!("{summary}\n\n")); } output.push_str(body); }
    }
    if part.attachment_omitted { output.push_str("> An oversized attachment path was omitted.\n\n"); }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_follow_preserves_a_word_across_caught_up_polls() {
        let part=|id:&str,text:&str,offset|crate::agent_execution::TranscriptPart{item:crate::chat::Item{id:id.into(),role:crate::chat::Role::Assistant,content:crate::chat::ItemContent::Text{text:text.into(),thinking:false,phase:crate::chat::Phase::Final},at:None,started_at:None},offset,complete:true,attachment_omitted:false};
        let mut renderer=Markdown::default();
        let mut output=renderer.part(&part("stream","Hel",0));
        output.push_str(&renderer.part(&part("stream","lo",3)));
        assert_eq!(output,"## Assistant\n\nHello");
        output.push_str(&renderer.part(&part("next","Next item",0)));
        output.push_str(renderer.finish());
        assert_eq!(output,"## Assistant\n\nHello\n\n## Assistant\n\nNext item\n\n");
    }
    #[test]
    fn markdown_follow_empty_placeholder_does_not_repeat_heading() {
        let mut part=crate::agent_execution::TranscriptPart{item:crate::chat::Item{id:"stream".into(),role:crate::chat::Role::Assistant,content:crate::chat::ItemContent::Text{text:String::new(),thinking:false,phase:crate::chat::Phase::Final},at:None,started_at:None},offset:0,complete:true,attachment_omitted:false};
        let mut renderer=Markdown::default();let mut output=renderer.part(&part);
        if let crate::chat::ItemContent::Text{text,..}=&mut part.item.content {*text="Hello".into();}
        output.push_str(&renderer.part(&part));output.push_str(renderer.finish());
        assert_eq!(output,"## Assistant\n\nHello\n\n");
    }

    #[test]
    fn transcript_arguments_and_markdown_preserve_user_and_assistant_text() {
        for args in [vec!["cagents", "agent", "transcript", "reviewer"], vec!["cagents", "agent", "transcript", "reviewer", "--follow", "--format", "json"]] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        let part = crate::agent_execution::TranscriptPart { item: crate::chat::Item {
            id: "message".into(), role: crate::chat::Role::Assistant, content: crate::chat::ItemContent::Text {
                text: "Readable transcript".into(), thinking: false, phase: crate::chat::Phase::Final }, at: None, started_at: None,
        }, offset: 0, complete: true, attachment_omitted: false };
        let mut renderer=Markdown::default();
        assert_eq!(format!("{}{}",renderer.part(&part),renderer.finish()), "## Assistant\n\nReadable transcript\n\n");
    }
}
