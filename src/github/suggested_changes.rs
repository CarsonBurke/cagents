//! Suggested replacements use the PR's exact head and a compare-and-swap
//! commit. No local checkout or uncommitted user file is modified.
use super::*;
use crate::repository::Oid;
use base64::Engine as _;

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion { pub replacement: String, pub before: u64, pub after: u64, pub explicit_range: bool }
#[derive(Debug, Clone)]
pub struct SuggestionRequest { pub comment: String, pub expected_body: String, pub index: usize, pub expected_head: String }

/// Only complete GitHub suggestion fences are actionable. Other code blocks
/// and malformed/overflowing offsets remain ordinary Markdown.
pub fn suggestions(body: &str) -> Vec<Suggestion> {
    let mut result = Vec::new(); let mut lines = body.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let fence = trimmed.bytes().take_while(|byte| *byte == b'`').count();
        if fence < 3 { continue; }
        let Some(header) = trimmed[fence..].strip_prefix("suggestion") else {
            // Skip ordinary fenced code so embedded suggestion-looking text
            // cannot turn a quoted example into an actionable replacement.
            for line in lines.by_ref() { if line.trim().bytes().all(|byte|byte==b'`') && line.trim().len()>=fence { break; } }
            continue;
        };
        let offsets = if header.is_empty() { Some((0, 0, false)) } else {
            header.strip_prefix(":-").and_then(|s| s.split_once('+'))
                .and_then(|(a,b)| Some((a.parse::<u64>().ok()?,b.parse::<u64>().ok()?,true)))
                .filter(|(a,b,_)| *a<=1000 && *b<=1000)
        };
        let mut replacement=Vec::new(); let mut complete=false;
        for line in lines.by_ref() {
            let trimmed=line.trim();
            if trimmed.len()>=fence && trimmed.bytes().all(|byte|byte==b'`') { complete=true; break; }
            replacement.push(line);
        }
        if let Some((before,after,explicit_range))=offsets && complete { result.push(Suggestion{replacement:replacement.join("\n"),before,after,explicit_range}); }
    }
    result
}

pub fn suggestion_block(lines: &str) -> String {
    let longest = lines.lines().map(|line|line.trim().bytes().take_while(|byte|*byte==b'`').count()).max().unwrap_or(0);
    let fence="`".repeat(3.max(longest+1));
    format!("{fence}suggestion\n{lines}\n{fence}")
}

pub fn replace_lines(source:&str,start:u64,end:u64,replacement:&str)->Result<String,GitHubError>{
    if source.len()>MAX_SOURCE_BYTES || source.contains('\0'){return Err(GitHubError::Inspection("Suggestions require a text file smaller than 2 MiB.".into()));}
    let lines:Vec<&str>=source.split_inclusive('\n').collect();
    let (Ok(start),Ok(end))=(usize::try_from(start),usize::try_from(end))else{return Err(GitHubError::Inspection("Suggestion line range is invalid.".into()));};
    if start==0 || start>end || end>lines.len(){return Err(GitHubError::Inspection("The suggested line range no longer exists.".into()));}
    let first=lines[..start-1].iter().map(|line|line.len()).sum::<usize>();let last=first+lines[start-1..end].iter().map(|line|line.len()).sum::<usize>();
    let eol=if lines[start-1].ends_with("\r\n"){ "\r\n" }else{"\n"};
    let mut value=String::with_capacity(source.len()+replacement.len());value.push_str(&source[..first]);
    if !replacement.is_empty(){value.push_str(&replacement.replace('\n',eol));if last<source.len()||source[..last].ends_with('\n'){value.push_str(eol);}}
    value.push_str(&source[last..]);Ok(value)
}

impl<E:ProcessExecutor> GitHubService<E>{
    fn suggestion_source(&self,key:&PullRequestKey,path:&str,expected_head:&str,require_author:bool,cancellation:&CancellationToken)->Result<(String,GitHubRepoKey,String),GitHubError>{
        key.validate()?;validate_review_path(path)?;Oid::parse(expected_head.to_owned()).map_err(|error|GitHubError::Inspection(error.to_string()))?;
        let data=self.graphql(&key.repository,"query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid headRefName state viewerDidAuthor headRepository{name owner{login} viewerPermission}}}}",Self::pull_request_variables(key),cancellation)?;
        let pr=&data["repository"]["pullRequest"];
        if pr["headRefOid"].as_str()!=Some(expected_head)||pr["state"].as_str()!=Some("OPEN"){return Err(GitHubError::Inspection("The PR head changed or the PR closed. Refresh before using this suggestion.".into()));}
        if require_author && pr["viewerDidAuthor"].as_bool()!=Some(true){return Err(GitHubError::Inspection("Only the PR author can apply suggestions here.".into()));}
        let head=&pr["headRepository"];
        if require_author && !matches!(head["viewerPermission"].as_str(),Some("WRITE"|"MAINTAIN"|"ADMIN")){return Err(GitHubError::Inspection("GitHub does not permit committing to the PR head repository.".into()));}
        let repository=GitHubRepoKey::new(&key.repository.host,key.repository.port,decode_value::<String>(&head["owner"]["login"])?,decode_value::<String>(&head["name"])?)?;
        let branch=decode_value::<String>(&pr["headRefName"])?;
        let data=self.graphql(&repository,"query($owner:String!,$name:String!,$head:String!,$path:String!){repository(owner:$owner,name:$name){object(expression:$head){... on Commit{file(path:$path){mode object{... on Blob{text isTruncated byteSize}}}}}}}",vec![("owner",repository.owner.clone().into()),("name",repository.name.clone().into()),("head",expected_head.into()),("path",path.into())],cancellation)?;
        let file=&data["repository"]["object"]["file"];
        if require_author && file["mode"].as_u64()!=Some(0o100644) { return Err(GitHubError::Inspection("This file's executable or symlink mode cannot be preserved by GitHub's suggestion commit API. Apply it in the checked-out worktree.".into())); }
        let blob=&file["object"];
        if blob["isTruncated"].as_bool()!=Some(false)||blob["byteSize"].as_u64().is_none_or(|size|size>MAX_SOURCE_BYTES as u64){return Err(GitHubError::Inspection("GitHub could not return the complete text file (maximum 2 MiB).".into()));}
        let text=decode_value::<String>(&blob["text"])?;Ok((text,repository,branch))
    }
    pub fn suggestion_lines(&self,key:&PullRequestKey,path:&str,expected_head:&str,start:u64,end:u64,cancellation:&CancellationToken)->Result<String,GitHubError>{
        let (text,_,_)=self.suggestion_source(key,path,expected_head,false,cancellation)?;
        let lines:Vec<&str>=text.lines().collect();let (Ok(start),Ok(end))=(usize::try_from(start),usize::try_from(end))else{return Err(GitHubError::Inspection("Invalid suggestion range.".into()));};
        if start==0||start>end||end>lines.len(){return Err(GitHubError::Inspection("The selected lines no longer exist.".into()));}
        let excerpt=lines[start-1..end].join("\n");
        if excerpt.len()>MAX_COMMENT_BODY_BYTES-128{return Err(GitHubError::Inspection("The selected lines exceed GitHub's comment size limit. Select a smaller range.".into()));}Ok(excerpt)
    }
    pub fn apply_suggestion(&self,key:&PullRequestKey,request:&SuggestionRequest,cancellation:&CancellationToken)->Result<ReviewMutationOutcome<()>,GitHubError>{
        validate_node_id("comment_id",&request.comment)?;
        let conversation=self.conversation(key,cancellation)?;
        let (thread,comment)=conversation.threads.iter().find_map(|thread|thread.comments.iter().find(|comment|comment.id.as_deref()==Some(&request.comment)).map(|comment|(thread,comment))).ok_or_else(||GitHubError::Inspection("The suggestion is no longer available. Refresh the discussion.".into()))?;
        if comment.body != request.expected_body { return Err(GitHubError::Inspection("The comment changed. Refresh before applying its suggestion.".into())); }
        if thread.outdated||thread.side!=ReviewDiffSide::Right||comment.is_pending(){return Err(GitHubError::Inspection("Only published suggestions on the current right side can be applied.".into()));}
        let suggestion=suggestions(&comment.body).into_iter().nth(request.index).ok_or_else(||GitHubError::Inspection("The suggestion changed. Refresh the discussion.".into()))?;
        let line=thread.line.ok_or_else(||GitHubError::Inspection("The suggestion has no current line anchor.".into()))?;
        let (start,end)=if suggestion.explicit_range{(line.checked_sub(suggestion.before),line.checked_add(suggestion.after))}else{(Some(thread.start_line.unwrap_or(line)),Some(line))};
        let (Some(start),Some(end))=(start,end)else{return Err(GitHubError::Inspection("Invalid suggestion range.".into()));};
        let (source,repository,branch)=self.suggestion_source(key,&thread.path,&request.expected_head,true,cancellation)?;
        let replacement=replace_lines(&source,start,end,&suggestion.replacement)?;
        if source==replacement{return Err(GitHubError::Inspection("This suggestion is already present in the PR head.".into()));}
        let query="mutation($input:CreateCommitOnBranchInput!){createCommitOnBranch(input:$input){commit{oid url}}}";
        let input=serde_json::json!({"query":query,"variables":{"input":{"branch":{"repositoryNameWithOwner":format!("{}/{}",repository.owner,repository.name),"branchName":branch},"expectedHeadOid":request.expected_head,"message":{"headline":format!("Apply review suggestion to {}",thread.path)},"fileChanges":{"additions":[{"path":thread.path,"contents":base64::engine::general_purpose::STANDARD.encode(replacement.as_bytes())}]}}}});
        let mut command=self.gh_command([OsString::from("api"),"graphql".into(),"--hostname".into(),repository.authority().into(),"--input".into(),"-".into()]);command.stdin=Some(input.to_string().into_bytes());
        self.mutate_command(&repository,&command,"/createCommitOnBranch",cancellation).map(|outcome|outcome.map(|_|()))
    }
}

#[cfg(test)]mod tests{use super::*;
use crate::repository::Oid;
#[test]fn suggestion_fences_and_offsets_are_explicit(){assert_eq!(suggestions("```rust\nx\n```\n```suggestion:-1+2\nnew\n```"),vec![Suggestion{replacement:"new".into(),before:1,after:2,explicit_range:true}]);assert!(suggestions("```suggestion\nunfinished").is_empty());assert!(suggestions("```suggestion:-99999+2\nx\n```").is_empty());}
#[test]fn inserted_suggestions_preserve_embedded_fences(){let source="before\n```rust\nx\n```\nafter";let block=suggestion_block(source);assert!(block.starts_with("````suggestion"));assert_eq!(suggestions(&block)[0].replacement,source);assert!(suggestions("````text\n```suggestion\nnot a suggestion\n```\n````").is_empty());}
#[test]fn replacements_preserve_context_line_endings_and_eof(){assert_eq!(replace_lines("a\nb\nc\n",2,2,"x\ny").unwrap(),"a\nx\ny\nc\n");assert_eq!(replace_lines("a\r\nb\r\n",2,2,"x").unwrap(),"a\r\nx\r\n");assert_eq!(replace_lines("a\nb",2,2,"x").unwrap(),"a\nx");assert_eq!(replace_lines("a\nb\nc\n",2,2,"").unwrap(),"a\nc\n");assert!(replace_lines("a\n",0,1,"x").is_err());}
}
