//! Bare document references gain the same scoped open action as Markdown links.
//! This operates on rendered prose spans, leaving code and existing links alone.
use super::*;
use iced::widget::text::Span;

fn destination(token:&str)->Option<String> {
    if token.starts_with("http://") || token.starts_with("https://") {
        let url=Url::parse(token).ok()?;
        return url.host_str().is_some().then(||token.to_owned());
    }
    if token.starts_with("file://") {
        let url=Url::parse(token).ok()?;
        return (!url.path().is_empty()).then(||token.to_owned());
    }
    if token.starts_with("www.") {
        let url=Url::parse(&format!("https://{token}")).ok()?;
        return url.host_str().is_some().then(||url.to_string());
    }
    let (before,last)=token.rsplit_once(':')?;
    if last.parse::<u32>().ok()? == 0 {return None;}
    let path=match before.rsplit_once(':') {
        Some((path,line)) if line.parse::<u32>().is_ok_and(|line|line>0)=>path,
        _=>before,
    };
    if path.contains("://") || path.contains(':') || path.is_empty() {return None;}
    let filename=path.rsplit('/').next()?;
    // Require an actual path or an extension, rather than making timestamps,
    // issue numbers and arbitrary colon-separated prose into file links.
    (path.contains('/') || (filename.starts_with('.') && filename.len()>1) || filename.rsplit_once('.').is_some_and(|(stem,ext)|!stem.is_empty() && !ext.is_empty()))
        .then(||token.to_owned())
}

fn token_end(text:&str)->usize {
    let mut end=text.len();
    while let Some(ch)=text[..end].chars().next_back() {
        let trim=matches!(ch,'.'|','|';'|'!'|'?'|']'|'}')
            || (ch==')' && text[..end].bytes().filter(|byte|*byte==b')').count()
                > text[..end].bytes().filter(|byte|*byte==b'(').count());
        if !trim {break;}
        end-=ch.len_utf8();
    }
    end
}

pub(super) fn spans(source:Arc<[Span<'static,String>]>,color:iced::Color)->Arc<[Span<'static,String>]> {
    let mut result=Vec::new();let mut changed=false;
    for span in source.iter() {
        if span.link.is_some() || span.font.is_some_and(|font|font.family==iced::font::Family::Monospace) {
            result.push(span.clone());continue;
        }
        let text=span.text.as_ref();let mut copied=0;
        for (start,token) in text.split_inclusive(|ch:char|ch.is_whitespace() || matches!(ch,'<'|'>'|'"'|'\''))
            .scan(0,|offset,part|{let start=*offset;*offset+=part.len();Some((start,part))}) {
            let token=token.trim_end_matches(|ch:char|ch.is_whitespace() || matches!(ch,'<'|'>'|'"'|'\''));
            let leading=token.trim_start_matches(['(','[','{']);let start=start+token.len()-leading.len();
            let end=start+token_end(leading);
            let Some(uri)=destination(&text[start..end]) else {continue;};
            if copied<start {let mut plain=span.clone();plain.text=text[copied..start].to_owned().into();result.push(plain);}
            let mut linked=span.clone();linked.text=text[start..end].to_owned().into();linked.link=Some(uri);linked.color=Some(color);
            result.push(linked);copied=end;changed=true;
        }
        if copied==0 {result.push(span.clone());}
        else if copied<text.len() {let mut plain=span.clone();plain.text=text[copied..].to_owned().into();result.push(plain);}
    }
    if changed {result.into()} else {source}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prose_links_keep_exact_text_unicode_punctuation_and_existing_targets() {
        let original="See (https://example.com/a_(b)), src/main.rs:42:7. 🦀 docs/guide.md:9 and www.example.com.";
        let linked=spans(vec![Span::new(original)].into(),iced::Color::WHITE);
        assert_eq!(linked.iter().map(|span|span.text.as_ref()).collect::<String>(),original);
        assert_eq!(linked.iter().filter_map(|span|span.link.as_deref()).collect::<Vec<_>>(),
            vec!["https://example.com/a_(b)","src/main.rs:42:7","docs/guide.md:9","https://www.example.com/"]);
        let existing=Span::new("https://display.example").link("https://destination.example");
        let code=Span::new("src/code.rs:22").font(iced::Font::MONOSPACE);
        let source:Arc<[Span<'static,String>]>=vec![existing,code,Span::new("time 12:30 issue:42")].into();
        assert!(Arc::ptr_eq(&source,&spans(source.clone(),iced::Color::WHITE)));
    }
}
