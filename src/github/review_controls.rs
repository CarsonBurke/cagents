//! Review completion controls and mutations, with exact-head merge fencing.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod { Merge, Squash, Rebase }
impl MergeMethod {
    pub fn api_name(self) -> &'static str { match self { Self::Merge => "MERGE", Self::Squash => "SQUASH", Self::Rebase => "REBASE" } }
}
impl std::fmt::Display for MergeMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(match self { Self::Merge=>"Merge commit", Self::Squash=>"Squash", Self::Rebase=>"Rebase" }) }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer { pub id: String, pub label: String, pub team: bool, pub bot: bool }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewControls {
    pub pull_request_id: String,
    pub head_oid: String,
    pub allowed_methods: Vec<MergeMethod>,
    pub merge_blocked: Option<String>,
    pub auto_merge_allowed: bool,
    pub auto_merge_enabled: bool,
    pub can_enable_auto_merge: bool,
    pub can_disable_auto_merge: bool,
    pub can_update: bool,
    pub is_draft: bool,
    pub requested_reviewers: Vec<Reviewer>,
    pub reviewers_complete: bool,
    pub suggested_title: String,
    pub suggested_body: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewControlMutation {
    Merge { method: MergeMethod, title: String, body: String, expected_head: String },
    AutoMerge { method: MergeMethod, enabled: bool },
    Draft(bool),
    /// Replace the requested set, allowing removal as well as addition.
    Reviewers(Vec<Reviewer>),
    EditReviewBody { review: String, body: String },
    EditIssueComment { comment: String, body: String },
}

impl<E: ProcessExecutor> GitHubService<E> {
    pub fn review_controls(&self, key: &PullRequestKey, cancellation: &CancellationToken) -> Result<ReviewControls, GitHubError> {
        key.validate()?;
        let query="query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){viewerPermission mergeCommitAllowed squashMergeAllowed rebaseMergeAllowed autoMergeAllowed pullRequest(number:$number){id headRefOid state isDraft mergeable mergeStateStatus reviewDecision viewerCanUpdate viewerCanEnableAutoMerge viewerCanDisableAutoMerge viewerMergeHeadlineText viewerMergeBodyText autoMergeRequest{mergeMethod} reviewRequests(first:100){nodes{requestedReviewer{__typename ...on User{id login} ...on Bot{id login} ...on Team{id name slug}}} pageInfo{hasNextPage}}}}}";
        let data=self.graphql(&key.repository,query,Self::pull_request_variables(key),cancellation)?;
        let repository=data.get("repository").ok_or_else(||GitHubError::Inspection("Repository unavailable".into()))?;
        let pr=repository.get("pullRequest").filter(|v|!v.is_null()).ok_or_else(||GitHubError::Inspection("Pull request unavailable".into()))?;
        let flag=|v:&serde_json::Value,name:&str|v.get(name).and_then(serde_json::Value::as_bool).unwrap_or(false);
        let text=|name:&str|pr.get(name).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let mut allowed_methods=Vec::new();
        for (name,method) in [("mergeCommitAllowed",MergeMethod::Merge),("squashMergeAllowed",MergeMethod::Squash),("rebaseMergeAllowed",MergeMethod::Rebase)] { if flag(repository,name){allowed_methods.push(method);} }
        let permission=repository.get("viewerPermission").and_then(serde_json::Value::as_str);
        let merge_blocked=if !permission.is_some_and(|permission|matches!(permission,"ADMIN"|"MAINTAIN"|"WRITE")){Some("You do not have permission to merge into this repository.".into())}
            else if text("state")!="OPEN" {Some("This pull request is no longer open.".into())}
            else if flag(pr,"isDraft"){Some("Mark this draft ready for review before merging.".into())}
            else if text("mergeable")=="CONFLICTING" || text("mergeStateStatus")=="DIRTY" {Some("Merge conflicts must be resolved before merging.".into())}
            else if ["BLOCKED","BEHIND","UNKNOWN"].contains(&text("mergeStateStatus").as_str()) {Some(match text("reviewDecision").as_str(){"CHANGES_REQUESTED"=>"A reviewer has requested changes.","REVIEW_REQUIRED"=>"Required approving reviews are missing.",_=>"Required checks, reviews, or branch updates are incomplete."}.into())}
            else if allowed_methods.is_empty(){Some("The repository does not allow any merge method.".into())} else {None};
        let nodes:Vec<_>=pr.pointer("/reviewRequests/nodes").and_then(serde_json::Value::as_array).into_iter().flatten().collect();
        let requested_reviewers:Vec<_>=nodes.iter().filter_map(|node|node.get("requestedReviewer")).filter_map(reviewer).collect();
        let reviewers_complete=requested_reviewers.len()==nodes.len() && pr.pointer("/reviewRequests/pageInfo/hasNextPage").and_then(serde_json::Value::as_bool)!=Some(true);
        Ok(ReviewControls{pull_request_id:text("id"),head_oid:text("headRefOid"),allowed_methods,merge_blocked,
            auto_merge_allowed:flag(repository,"autoMergeAllowed"),auto_merge_enabled:pr.get("autoMergeRequest").is_some_and(|v|!v.is_null()),
            can_enable_auto_merge:flag(pr,"viewerCanEnableAutoMerge"),can_disable_auto_merge:flag(pr,"viewerCanDisableAutoMerge"),can_update:flag(pr,"viewerCanUpdate"),is_draft:flag(pr,"isDraft"),requested_reviewers,reviewers_complete,
            suggested_title:text("viewerMergeHeadlineText"),suggested_body:text("viewerMergeBodyText")})
    }

    pub fn mutate_review_control(&self, repository:&GitHubRepoKey, pull_request_id:&str, mutation:&ReviewControlMutation, cancellation:&CancellationToken) -> Result<ReviewMutationOutcome<()>,GitHubError> {
        validate_node_id("pull_request_id",pull_request_id)?;
        let mut variables=vec![("id",pull_request_id.into())];
        let (query,payload):(String,&str)=match mutation {
            ReviewControlMutation::Merge{method,title,body,expected_head}=>{
                if *method==MergeMethod::Rebase {
                    variables.extend([("method",method.api_name().into()),("head",expected_head.as_str().into())]);
                    return self.mutate(repository,"mutation($id:ID!,$method:PullRequestMergeMethod!,$head:GitObjectID!){mergePullRequest(input:{pullRequestId:$id,mergeMethod:$method,expectedHeadOid:$head}){pullRequest{id}}}",variables,"/mergePullRequest/pullRequest",cancellation).map(|outcome|outcome.map(drop));
                }
                validate_text("title",title)?; validate_text("body",body)?;
                variables.extend([("method",method.api_name().into()),("title",title.as_str().into()),("body",body.as_str().into()),("head",expected_head.as_str().into())]);
                ("mutation($id:ID!,$method:PullRequestMergeMethod!,$title:String!,$body:String!,$head:GitObjectID!){mergePullRequest(input:{pullRequestId:$id,mergeMethod:$method,commitHeadline:$title,commitBody:$body,expectedHeadOid:$head}){pullRequest{id}}}".into(),"/mergePullRequest/pullRequest")
            }
            ReviewControlMutation::AutoMerge{method,enabled:true}=>{
                variables.push(("method",method.api_name().into()));
                ("mutation($id:ID!,$method:PullRequestMergeMethod!){enablePullRequestAutoMerge(input:{pullRequestId:$id,mergeMethod:$method}){pullRequest{id}}}".into(),"/enablePullRequestAutoMerge/pullRequest")
            }
            ReviewControlMutation::AutoMerge{enabled:false,..}=>("mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){pullRequest{id}}}".into(),"/disablePullRequestAutoMerge/pullRequest"),
            ReviewControlMutation::Draft(true)=>("mutation($id:ID!){convertPullRequestToDraft(input:{pullRequestId:$id}){pullRequest{id}}}".into(),"/convertPullRequestToDraft/pullRequest"),
            ReviewControlMutation::Draft(false)=>("mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{id}}}".into(),"/markPullRequestReadyForReview/pullRequest"),
            ReviewControlMutation::Reviewers(reviewers)=>{
                if reviewers.len()>100{return Err(GitHubError::Inspection("Select at most 100 reviewers per request.".into()));}
                let mut declarations=String::from("$id:ID!");let mut users=Vec::new();let mut teams=Vec::new();let mut bots=Vec::new();
                // Only generated variable names enter GraphQL source; every ID
                // stays a typed variable, including team and enterprise IDs.
                let mut names=Vec::new();for index in 0..reviewers.len(){names.push(format!("reviewer{index}"));}
                // Variables borrow their names, so construct the document then
                // execute inside this arm while names are alive.
                for (index,selected) in reviewers.iter().enumerate(){validate_node_id("reviewer",&selected.id)?;declarations.push_str(&format!(",${}:ID!",names[index]));if selected.team{teams.push(format!("${}",names[index]));}else if selected.bot{bots.push(format!("${}",names[index]));}else{users.push(format!("${}",names[index]));}}
                let query=format!("mutation({declarations}){{requestReviews(input:{{pullRequestId:$id,userIds:[{}],teamIds:[{}],botIds:[{}],union:false}}){{pullRequest{{id}}}}}}",users.join(","),teams.join(","),bots.join(","));
                for (name,selected) in names.iter().zip(reviewers){variables.push((name.as_str(),selected.id.as_str().into()));}
                return self.mutate(repository,&query,variables,"/requestReviews/pullRequest",cancellation).map(|result|result.map(drop));
            }
            ReviewControlMutation::EditReviewBody{review,body}=>{
                validate_node_id("review_id",review)?;validate_comment_body(body)?;variables=vec![("id",review.as_str().into()),("body",body.as_str().into())];
                ("mutation($id:ID!,$body:String!){updatePullRequestReview(input:{pullRequestReviewId:$id,body:$body}){pullRequestReview{id}}}".into(),"/updatePullRequestReview/pullRequestReview")
            }
            ReviewControlMutation::EditIssueComment{comment,body}=>{
                validate_node_id("comment_id",comment)?;validate_comment_body(body)?;variables=vec![("id",comment.as_str().into()),("body",body.as_str().into())];
                ("mutation($id:ID!,$body:String!){updateIssueComment(input:{id:$id,body:$body}){issueComment{id}}}".into(),"/updateIssueComment/issueComment")
            }
        };
        self.mutate(repository,&query,variables,payload,cancellation).map(|outcome|outcome.map(drop))
    }
}
fn reviewer(value:&serde_json::Value)->Option<Reviewer>{Some(Reviewer{id:value.get("id")?.as_str()?.into(),label:value.get("login").or_else(||value.get("slug"))?.as_str()?.into(),team:value.get("slug").is_some(),bot:value.get("__typename").and_then(serde_json::Value::as_str)==Some("Bot")})}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewerPage { pub reviewers: Vec<Reviewer>, pub next_cursor: Option<String> }
impl<E:ProcessExecutor> GitHubService<E> {
    pub fn reviewer_candidates(&self, repository:&GitHubRepoKey, search:&str, cursor:Option<&str>, teams:bool, cancellation:&CancellationToken)->Result<ReviewerPage,GitHubError>{
        repository.validate()?;
        let (query,path)=if teams {
            ("query($owner:String!,$searchText:String!,$cursor:String){organization(login:$owner){teams(first:50,after:$cursor,query:$searchText){nodes{id name slug} pageInfo{hasNextPage endCursor}}}}","/organization/teams")
        }else{
            ("query($owner:String!,$name:String!,$searchText:String!,$cursor:String){repository(owner:$owner,name:$name){assignableUsers(first:50,after:$cursor,query:$searchText){nodes{id login} pageInfo{hasNextPage endCursor}}}}","/repository/assignableUsers")
        };
        let mut variables=vec![("owner",repository.owner.as_str().into()),("searchText",search.into())];
        if !teams {variables.push(("name",repository.name.as_str().into()));} if let Some(cursor)=cursor{variables.push(("cursor",cursor.into()));}
        let data=self.graphql(repository,query,variables,cancellation)?;
        let Some(connection)=data.pointer(path).filter(|v|!v.is_null())else{return Ok(ReviewerPage{reviewers:Vec::new(),next_cursor:None});};
        let connection:Connection<serde_json::Value>=decode_value(connection)?;
        let next_cursor=if connection.page_info.has_next_page{Some(next_cursor(&connection.page_info,cursor)?)}else{None};
        Ok(ReviewerPage{reviewers:connection.nodes.iter().filter_map(reviewer).collect(),next_cursor})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Recording { calls: Mutex<Vec<CommandSpec>>, response: serde_json::Value }
    impl ProcessExecutor for Recording {
        fn execute(&self, command:&CommandSpec, _: &CancellationToken)->Result<ProcessOutput,ProcessFailure>{
            self.calls.lock().unwrap().push(command.clone());
            Ok(ProcessOutput{success:true,exit_code:Some(0),signal:None,stdout:self.response.to_string().into_bytes(),stderr:Vec::new()})
        }
    }
    fn repository()->GitHubRepoKey{GitHubRepoKey::new("controls.invalid",None,"owner","repo").unwrap()}
    fn invoke(mutation:ReviewControlMutation,payload:&str)->Vec<String>{
        let mut data=serde_json::json!({});data[payload]=serde_json::json!({"pullRequest":{"id":"PR_fixture"},"pullRequestReview":{"id":"review"},"issueComment":{"id":"comment"}});
        let executor=Arc::new(Recording{calls:Mutex::new(Vec::new()),response:serde_json::json!({"data":data})});
        let service=GitHubService::new("gh",executor.clone());let api:&dyn GitHubReviewApi=&service;
        assert!(matches!(api.mutate_review_control(&repository(),"PR_fixture",&mutation,&CancellationToken::new()).unwrap(),ReviewMutationOutcome::Applied(())));
        executor.calls.lock().unwrap()[0].arguments.iter().map(|argument|argument.to_string_lossy().into_owned()).collect()
    }
    #[test]fn merge_methods_and_exact_head_are_typed(){
        let args=invoke(ReviewControlMutation::Merge{method:MergeMethod::Squash,title:"User's title".into(),body:"line one\nline two".into(),expected_head:"a".repeat(40)},"mergePullRequest");
        assert!(args.contains(&"method=SQUASH".into()));assert!(args.contains(&format!("head={}","a".repeat(40))));assert!(args.contains(&"title=User's title".into()));assert!(args.contains(&"body=line one\nline two".into()));
        let rebase=invoke(ReviewControlMutation::Merge{method:MergeMethod::Rebase,title:String::new(),body:String::new(),expected_head:"b".repeat(40)},"mergePullRequest");assert!(!rebase.iter().any(|argument|argument.starts_with("title=")));
    }
    #[test]fn draft_and_auto_merge_mutations_are_distinct(){
        for (mutation,payload) in [(ReviewControlMutation::Draft(true),"convertPullRequestToDraft"),(ReviewControlMutation::Draft(false),"markPullRequestReadyForReview"),(ReviewControlMutation::AutoMerge{method:MergeMethod::Squash,enabled:true},"enablePullRequestAutoMerge"),(ReviewControlMutation::AutoMerge{method:MergeMethod::Merge,enabled:false},"disablePullRequestAutoMerge")]{let args=invoke(mutation,payload);assert!(args.iter().any(|argument|argument.starts_with("query=")&&argument.contains(payload)));}
    }
    #[test]fn reviewer_replace_preserves_typed_user_team_bot_ids_and_can_clear(){
        let args=invoke(ReviewControlMutation::Reviewers(vec![Reviewer{id:"U_fixture".into(),label:"user".into(),team:false,bot:false},Reviewer{id:"T_fixture".into(),label:"team".into(),team:true,bot:false},Reviewer{id:"B_fixture".into(),label:"bot".into(),team:false,bot:true}]),"requestReviews");let query=args.iter().find(|argument|argument.starts_with("query=")).unwrap();assert!(query.contains("userIds:[$reviewer0],teamIds:[$reviewer1],botIds:[$reviewer2],union:false"));assert!(!query.contains("U_fixture"));assert!(args.contains(&"reviewer1=T_fixture".into()));let cleared=invoke(ReviewControlMutation::Reviewers(Vec::new()),"requestReviews");assert!(cleared.iter().any(|argument|argument.contains("userIds:[],teamIds:[],botIds:[]")));
    }
    #[test]fn published_body_edits_keep_body_out_of_document(){
        for (mutation,payload) in [(ReviewControlMutation::EditIssueComment{comment:"C_fixture".into(),body:"body with $query".into()},"updateIssueComment"),(ReviewControlMutation::EditReviewBody{review:"R_fixture".into(),body:"body with $query".into()},"updatePullRequestReview")]{let args=invoke(mutation,payload);assert!(args.contains(&"body=body with $query".into()));assert!(!args.iter().find(|argument|argument.starts_with("query=")).unwrap().contains("body with"));}
    }
    #[test]fn typed_refusal_does_not_report_a_success(){
        let executor=Arc::new(Recording{calls:Mutex::new(Vec::new()),response:serde_json::json!({"errors":[{"type":"FORBIDDEN","message":"permission denied"}],"data":{"convertPullRequestToDraft":null}})});let service=GitHubService::new("gh",executor);assert!(matches!(service.mutate_review_control(&repository(),"PR_fixture",&ReviewControlMutation::Draft(true),&CancellationToken::new()).unwrap(),ReviewMutationOutcome::NotApplied{..}));
    }
}
