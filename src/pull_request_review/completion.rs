//! Explicit review-completion actions. Reads and writes stay on workers;
//! delayed responses belong to the review generation that requested them.
use super::*;
use crate::github::review_controls::{MergeMethod, ReviewControlMutation, ReviewControls, Reviewer, ReviewerPage};
use cosmic::iced::widget::text_editor;

#[derive(Debug, Clone)]
pub enum Action {
    Toggle,
    Reload,
    Loaded { generation:u64, request:u64, result:Result<ReviewControls,String> },
    Method(usize),
    Title(String),
    Body(text_editor::Action),
    Merge,
    AutoMerge(bool),
    Draft(bool),
    Reviewers,
    TeamTab(bool),
    Query(String),
    Search { generation:u64, request:u64 },
    More,
    Candidates { generation:u64, request:u64, append:bool, result:Result<ReviewerPage,String> },
    Pick(Reviewer),
    SaveReviewers,
    EditPublished { review: bool, id: String },
    PublishedBody(text_editor::Action),
    CancelPublished,
    SavePublished,
    Finished { generation:u64, result:Result<ReviewMutationOutcome<()>,String> },
}
impl Action { pub fn background(&self)->bool{matches!(self,Self::Loaded{..}|Self::Candidates{..}|Self::Finished{..}|Self::Search{..})} }

pub(super) struct Controls {
    pub open:bool,
    pub loading:bool,
    load_id:u64,
    pub busy:bool,
    data:Option<ReviewControls>,
    error:Option<String>,
    uncertain:bool,
    method:usize,
    title:String,
    body:text_editor::Content<cosmic::Renderer>,
    edited:bool,
    reviewers_open:bool,
    reviewers_edited:bool,
    selected:BTreeMap<String,Reviewer>,
    teams:bool,
    query:String,
    request:u64,
    candidates:Vec<Reviewer>,
    next:Option<String>,
    candidates_loading:bool,
    cancellation:CancellationToken,
    published:Option<PublishedEdit>,
}
impl Default for Controls {
    fn default()->Self{Self{open:false,loading:false,load_id:0,busy:false,data:None,error:None,uncertain:false,method:0,title:String::new(),body:text_editor::Content::new(),edited:false,reviewers_open:false,reviewers_edited:false,selected:BTreeMap::new(),teams:false,query:String::new(),request:0,candidates:Vec::new(),next:None,candidates_loading:false,cancellation:CancellationToken::new(),published:None}}
}
struct PublishedEdit { review: bool, id: String, original: String, content: text_editor::Content<cosmic::Renderer> }
impl Controls {
    pub fn blocker(&self)->Option<&'static str>{
        if self.busy{Some("Wait for the PR action to finish before leaving this review.")}
        else if self.uncertain{Some("Check the unconfirmed PR action on GitHub, then refresh PR actions before leaving this review.")}
        else if self.published.is_some(){Some("Save or cancel the published comment edit before leaving this review.")}
        else{None}
    }
    pub fn reset(&mut self){self.cancellation.cancel();*self=Self::default();}
    fn method(&self)->Option<MergeMethod>{self.data.as_ref()?.allowed_methods.get(self.method).copied()}
}

impl State {
    pub(super) fn update_completion(&mut self,action:Action)->ActionTask {
        match action {
            Action::Toggle=>{self.completion.open=!self.completion.open;if self.completion.open && self.completion.data.is_none(){return self.load_completion();}}
            Action::Reload=>{return self.load_completion();}
            Action::Loaded{generation,request,result}=>{
                if request!=self.completion.load_id || self.completion.busy || generation!=self.generation{return ActionTask::none();}self.completion.loading=false;
                match result {
                    Ok(data)=>{
                        self.completion.uncertain=false;
                        if !self.completion.edited{self.completion.title=data.suggested_title.clone();self.completion.body=text_editor::Content::with_text(&data.suggested_body);}
                        if !self.completion.reviewers_edited{self.completion.selected=data.requested_reviewers.iter().cloned().map(|r|(r.id.clone(),r)).collect();}
                        if self.completion.method>=data.allowed_methods.len(){self.completion.method=0;}
                        self.completion.data=Some(data);self.completion.error=None;
                    }
                    Err(error)=>self.completion.error=Some(error),
                }
            }
            Action::Method(index)=>{if self.completion.data.as_ref().is_some_and(|data|index<data.allowed_methods.len()){self.completion.method=index;}}
            Action::Title(title)=>{self.completion.title=title;self.completion.edited=true;}
            Action::Body(action)=>{if action.is_edit(){self.completion.edited=true;}self.completion.body.perform(action);}
            Action::Merge=>{
                let Some(data)=self.completion.data.as_ref()else{return ActionTask::none();};
                if data.merge_blocked.is_some() || self.context.as_ref().is_none_or(|context|context.pending.is_some() || context.head_oid != data.head_oid) || self.discussion.composer.is_some(){return ActionTask::none();}
                let Some(method)=self.completion.method()else{return ActionTask::none();};
                return self.mutate_completion(ReviewControlMutation::Merge{method,title:self.completion.title.clone(),body:self.completion.body.text(),expected_head:data.head_oid.clone()});
            }
            Action::AutoMerge(enabled)=>{if enabled && (self.context.as_ref().is_none_or(|context|context.pending.is_some()||self.completion.data.as_ref().is_none_or(|data|data.head_oid!=context.head_oid))||self.discussion.composer.is_some()){return ActionTask::none();}let Some(method)=self.completion.method()else{return ActionTask::none();};return self.mutate_completion(ReviewControlMutation::AutoMerge{method,enabled});}
            Action::Draft(draft)=>return self.mutate_completion(ReviewControlMutation::Draft(draft)),
            Action::Reviewers=>{self.completion.reviewers_open=!self.completion.reviewers_open;if self.completion.reviewers_open{return self.load_reviewer_candidates(false);}}
            Action::TeamTab(teams)=>{self.completion.teams=teams;self.completion.query.clear();self.completion.next=None;self.completion.cancellation.cancel();self.completion.candidates_loading=false;return self.load_reviewer_candidates(false);}
            Action::Query(query)=>{
                self.completion.query=query;self.completion.next=None;self.completion.request=self.completion.request.wrapping_add(1);self.completion.cancellation.cancel();self.completion.candidates_loading=false;
                let generation=self.generation;let request=self.completion.request;
                return iced::Task::perform(async move{tokio::time::sleep(Duration::from_millis(350)).await;Action::Search{generation,request}},|action|super::Action::Completion(action));
            }
            Action::Search{generation,request}=>{if generation==self.generation && request==self.completion.request{return self.load_reviewer_candidates(false);}}
            Action::More=>return self.load_reviewer_candidates(true),
            Action::Candidates{generation,request,append,result}=>{
                if generation!=self.generation || request!=self.completion.request{return ActionTask::none();}self.completion.candidates_loading=false;
                match result {
                    Ok(page)=>{
                        self.completion.next=page.next_cursor;if !append{self.completion.candidates.clear();}
                        let author=self.target.as_ref().and_then(|target|target.pull_request.author.as_ref()).map(|author|author.login.as_str());
                        for reviewer in page.reviewers{if !reviewer.team && author.is_some_and(|author|author.eq_ignore_ascii_case(&reviewer.label)){continue;}if !self.completion.candidates.iter().any(|current|current.id==reviewer.id){self.completion.candidates.push(reviewer);}}
                    }
                    Err(error)=>self.completion.error=Some(error),
                }
            }
            Action::Pick(reviewer)=>{if self.completion.selected.remove(&reviewer.id).is_none(){self.completion.selected.insert(reviewer.id.clone(),reviewer);}self.completion.reviewers_edited=true;}
            Action::SaveReviewers=>{
                if self.completion.data.as_ref().is_none_or(|data|!data.reviewers_complete){return ActionTask::none();}
                return self.mutate_completion(ReviewControlMutation::Reviewers(self.completion.selected.values().cloned().collect()));
            }
            Action::EditPublished{review,id}=>{
                if self.completion.published.is_some(){self.completion.error=Some("Save or cancel the open published comment edit first.".into());return ActionTask::none();}
                let Some(conversation)=&self.discussion.conversation else{return ActionTask::none();};
                let comment=if review{conversation.reviews.iter().map(|review|&review.comment).find(|comment|comment.id.as_ref()==Some(&id))}else{conversation.comments.iter().find(|comment|comment.id.as_ref()==Some(&id))};
                if let Some(comment)=comment.filter(|comment|comment.viewer_did_author&&comment.viewer_can_update&&!comment.is_pending()){
                    self.completion.published=Some(PublishedEdit{review,id,original:comment.body.clone(),content:text_editor::Content::with_text(&comment.body)});
                }
            }
            Action::PublishedBody(action)=>{if !self.completion.busy && let Some(editor)=&mut self.completion.published{editor.content.perform(action);}}
            Action::CancelPublished=>{if !self.completion.busy{self.completion.published=None;}}
            Action::SavePublished=>{
                let Some(editor)=&self.completion.published else{return ActionTask::none();};
                if editor.content.text().trim().is_empty(){return ActionTask::none();}
                let mutation=if editor.review{ReviewControlMutation::EditReviewBody{review:editor.id.clone(),body:editor.content.text()}}else{ReviewControlMutation::EditIssueComment{comment:editor.id.clone(),body:editor.content.text()}};
                return self.mutate_completion(mutation);
            }
            Action::Finished{generation,result}=>{
                if generation!=self.generation{return ActionTask::none();}self.completion.busy=false;
                match result {
                    Ok(ReviewMutationOutcome::Applied(()))=>{self.completion.error=None;self.completion.edited=false;self.completion.reviewers_edited=false;self.completion.published=None;self.notice=Some("Pull request updated on GitHub.".into());return ActionTask::batch([self.load_completion(),self.refresh()]);}
                    Ok(ReviewMutationOutcome::NotApplied{failure})=>self.completion.error=Some(failure.to_string()),
                    Ok(ReviewMutationOutcome::OutcomeUnknown{failure})=>{self.completion.uncertain=true;self.completion.error=Some(format!("GitHub may have applied the change. Check GitHub, then refresh before retrying. {failure}"));}
                    Err(error)=>self.completion.error=Some(error),
                }
            }
        }
        ActionTask::none()
    }

    fn load_completion(&mut self)->ActionTask {
        let Some(target)=&self.target else{return ActionTask::none();};
        if self.completion.busy{return ActionTask::none();}
        let key=PullRequestKey::new(target.repository.clone(),target.pull_request.number).expect("verified PR key");
        self.completion.loading=true;self.completion.load_id=self.completion.load_id.wrapping_add(1);let request=self.completion.load_id;let generation=self.generation;let api=Arc::clone(&self.api);let cancellation=self.cancellation.clone();
        iced::Task::perform(async move{
            let result=tokio::task::spawn_blocking(move||api.review_controls(&key,&cancellation).map_err(|error|error.to_string())).await.unwrap_or_else(|error|Err(error.to_string()));
            Action::Loaded{generation,request,result}
        },|action|super::Action::Completion(action))
    }
    fn load_reviewer_candidates(&mut self,append:bool)->ActionTask {
        let Some(target)=&self.target else{return ActionTask::none();};
        if self.completion.candidates_loading || (append && self.completion.next.is_none()){return ActionTask::none();}
        self.completion.cancellation.cancel();self.completion.cancellation=CancellationToken::new();self.completion.request=self.completion.request.wrapping_add(1);
        let request=self.completion.request;let generation=self.generation;let cancellation=self.completion.cancellation.clone();let api=Arc::clone(&self.api);let repository=target.repository.clone();
        let query=self.completion.query.clone();let teams=self.completion.teams;let cursor=if append{self.completion.next.clone()}else{None};self.completion.candidates_loading=true;
        iced::Task::perform(async move{
            let result=tokio::task::spawn_blocking(move||api.reviewer_candidates(&repository,&query,cursor.as_deref(),teams,&cancellation).map_err(|error|error.to_string())).await.unwrap_or_else(|error|Err(error.to_string()));
            Action::Candidates{generation,request,append,result}
        },|action|super::Action::Completion(action))
    }
    fn mutate_completion(&mut self,mutation:ReviewControlMutation)->ActionTask {
        if self.completion.busy || self.completion.uncertain || self.discussion.busy || self.discussion.unknown.is_some(){return ActionTask::none();}
        if self.completion.published.is_some() && !matches!(mutation,ReviewControlMutation::EditReviewBody{..}|ReviewControlMutation::EditIssueComment{..}) { self.completion.error=Some("Save or cancel the published comment edit first.".into());return ActionTask::none(); }
        let Some(target)=&self.target else{return ActionTask::none();};
        let key=PullRequestKey::new(target.repository.clone(),target.pull_request.number).expect("verified PR key");
        self.completion.load_id=self.completion.load_id.wrapping_add(1);self.completion.loading=false;
        let generation=self.generation;let api=Arc::clone(&self.api);let original=self.completion.published.as_ref().map(|edit|edit.original.clone());let expected_reviewers=self.completion.data.as_ref().map(|data|data.requested_reviewers.iter().map(|reviewer|reviewer.id.clone()).collect::<BTreeSet<_>>());self.completion.busy=true;self.completion.error=None;
        iced::Task::perform(async move{
            let result=tokio::task::spawn_blocking(move||{
                let cancellation=CancellationToken::new();let fresh=api.review_controls(&key,&cancellation).map_err(|error|error.to_string())?;
                match &mutation {
                    ReviewControlMutation::EditReviewBody{review,..}|ReviewControlMutation::EditIssueComment{comment:review,..}=>{
                        let conversation=api.conversation(&key,&cancellation).map_err(|error|error.to_string())?;
                        let comment=if matches!(mutation,ReviewControlMutation::EditReviewBody{..}){conversation.reviews.iter().map(|review|&review.comment).find(|comment|comment.id.as_ref()==Some(review))}else{conversation.comments.iter().find(|comment|comment.id.as_ref()==Some(review))};
                        if comment.is_none_or(|comment|!comment.viewer_did_author||!comment.viewer_can_update||comment.is_pending()||Some(&comment.body)!=original.as_ref()){return Err("The comment changed or can no longer be edited. Refresh before trying again.".into());}
                    }
                    ReviewControlMutation::Merge{method,expected_head,..}=>{let context=api.review_context(&key,&cancellation).map_err(|error|error.to_string())?;if context.pending.is_some(){return Err("Submit or discard your pending review before merging.".into());}if fresh.head_oid!=*expected_head{return Err("The PR head changed; refresh and review it before merging.".into());}if let Some(reason)=fresh.merge_blocked{return Err(reason);}if !fresh.allowed_methods.contains(method){return Err("This merge method is no longer allowed by the repository.".into());}}
                    ReviewControlMutation::Reviewers(_)=>{if !fresh.is_open||!fresh.can_update||!fresh.reviewers_complete{return Err("GitHub does not permit changing this complete requested reviewer set.".into());}if Some(fresh.requested_reviewers.iter().map(|reviewer|reviewer.id.clone()).collect::<BTreeSet<_>>())!=expected_reviewers{return Err("Reviewer requests changed. Refresh before editing the requested set.".into());}}
                    ReviewControlMutation::Draft(_)=>{if !fresh.is_open||!fresh.can_update{return Err("GitHub does not permit changing this pull request.".into());}}
                    ReviewControlMutation::AutoMerge{enabled:true,method}=>{let context=api.review_context(&key,&cancellation).map_err(|error|error.to_string())?;if context.pending.is_some(){return Err("Submit or discard your pending review before enabling auto-merge.".into());}if !fresh.is_open||!fresh.allowed_methods.contains(method)||!fresh.auto_merge_allowed || !fresh.can_enable_auto_merge{return Err("Auto-merge cannot be enabled for this pull request.".into());}}
                    ReviewControlMutation::AutoMerge{enabled:false,..}=>{if !fresh.can_disable_auto_merge{return Err("Auto-merge cannot be disabled for this pull request.".into());}}
                    _=>{}
                }
                api.mutate_review_control(&key.repository,&fresh.pull_request_id,&mutation,&cancellation).map_err(|error|error.to_string())
            }).await.unwrap_or_else(|error|Err(error.to_string()));Action::Finished{generation,result}
        },|action|super::Action::Completion(action))
    }

    pub(super) fn completion_view(&self,modal_open:bool)->Element<'_,super::Action>{
        let state=&self.completion;let mut rows:Vec<Element<'_,Action>>=Vec::new();let enabled=!modal_open && !state.busy && !state.loading && !state.uncertain && !self.discussion.busy && self.discussion.unknown.is_none();
        if let Some(error)=&state.error{rows.push(design::caption(error).into());}
        rows.push(crate::ui_controls::button::quiet(if state.uncertain{"I checked GitHub; refresh"}else{"Refresh PR actions"}).on_press_maybe((!modal_open&&!state.busy).then_some(Action::Reload)).into());
        if state.loading{rows.push(design::caption("Loading PR actions…").into());}
        if let Some(data)=&state.data{
            rows.push(crate::ui_controls::button::secondary(if data.is_draft{"Ready for review"}else{"Convert to draft"}).on_press_maybe((enabled&&data.is_open&&data.can_update).then_some(Action::Draft(!data.is_draft))).into());
            rows.push(crate::ui_controls::button::secondary("Request reviewers").on_press_maybe((enabled&&data.is_open).then_some(Action::Reviewers)).into());
            if !data.requested_reviewers.is_empty(){rows.push(design::caption(format!("Requested: {}",data.requested_reviewers.iter().map(|r|r.label.as_str()).collect::<Vec<_>>().join(", "))).into());}
            if state.reviewers_open{
                rows.push(widget::row::with_children(vec![crate::ui_controls::button::quiet("Users").on_press_maybe(enabled.then_some(Action::TeamTab(false))).into(),crate::ui_controls::button::quiet("Teams").on_press_maybe(enabled.then_some(Action::TeamTab(true))).into()]).spacing(space::XS).into());
                rows.push(crate::ui_controls::field::search(if state.teams{"Search teams"}else{"Search users"},&state.query).on_input(Action::Query).into());
                for reviewer in state.selected.values(){rows.push(crate::ui_controls::button::quiet(format!("Remove {}",reviewer.label)).on_press_maybe(enabled.then(||Action::Pick(reviewer.clone()))).into());}
                for reviewer in state.candidates.iter().filter(|r|!state.selected.contains_key(&r.id)){rows.push(crate::ui_controls::button::quiet(format!("Add {}",reviewer.label)).on_press_maybe(enabled.then(||Action::Pick(reviewer.clone()))).into());}
                if state.next.is_some(){rows.push(crate::ui_controls::button::quiet("Load more reviewers").on_press_maybe((enabled&&!state.candidates_loading).then_some(Action::More)).into());}
                if !data.reviewers_complete{rows.push(design::caption("The requested set is incomplete or includes a reviewer type this app cannot edit. Open GitHub to manage it.").into());}
                rows.push(crate::ui_controls::button::secondary("Save reviewer requests").on_press_maybe((enabled&&data.reviewers_complete&&state.reviewers_edited).then_some(Action::SaveReviewers)).into());
            }
            if let Some(conversation)=&self.discussion.conversation{
                for (review,comment) in conversation.comments.iter().map(|comment|(false,comment)).chain(conversation.reviews.iter().map(|review|(true,&review.comment))).filter(|(_,comment)|comment.viewer_did_author&&comment.viewer_can_update&&!comment.is_pending()){
                    if let Some(id)=&comment.id{rows.push(crate::ui_controls::button::quiet(format!("Edit your {} · {}",if review{"review summary"}else{"comment"},comment.created_at)).on_press_maybe((enabled&&state.published.is_none()).then(||Action::EditPublished{review,id:id.clone()})).into());}
                }
            }
            if let Some(editor)=&state.published{
                let mut body=text_editor::TextEditor::new(&editor.content).height(Length::Fixed(140.0));if enabled{body=body.on_action(Action::PublishedBody);}rows.push(body.into());
                rows.push(widget::row::with_children(vec![crate::ui_controls::button::primary("Save published comment").on_press_maybe((enabled&&!editor.content.text().trim().is_empty()).then_some(Action::SavePublished)).into(),crate::ui_controls::button::secondary("Cancel edit").on_press_maybe((!modal_open&&!state.busy).then_some(Action::CancelPublished)).into()]).spacing(space::S).into());
            }
            if let Some(reason)=&data.merge_blocked{rows.push(design::caption(reason).into());}
            if !data.allowed_methods.is_empty(){rows.push(crate::ui_controls::field::select(&data.allowed_methods,Some(state.method),Action::Method).into());}
            if state.method()==Some(MergeMethod::Squash){
                rows.push(crate::ui_controls::field::text("Commit title",&state.title).on_input(Action::Title).into());
                let mut body=text_editor::TextEditor::new(&state.body).height(Length::Fixed(96.0));if enabled{body=body.on_action(Action::Body);}rows.push(body.into());
            }
            let pending=self.context.as_ref().is_none_or(|context|context.pending.is_some()||context.head_oid!=data.head_oid)||self.discussion.composer.is_some();
            if pending{rows.push(design::caption("Submit or discard your pending review before merging.").into());}
            rows.push(crate::ui_controls::button::secondary("Merge pull request").on_press_maybe((enabled&&!pending&&data.merge_blocked.is_none()&&state.method().is_some()&&!state.title.trim().is_empty()).then_some(Action::Merge)).into());
            if data.auto_merge_enabled{rows.push(crate::ui_controls::button::secondary("Disable auto-merge").on_press_maybe((enabled&&data.can_disable_auto_merge).then_some(Action::AutoMerge(false))).into());}
            else if data.auto_merge_allowed{rows.push(crate::ui_controls::button::secondary("Enable auto-merge").on_press_maybe((enabled&&!pending&&data.can_enable_auto_merge&&state.method().is_some()).then_some(Action::AutoMerge(true))).into());}
            else{rows.push(design::caption("The repository does not allow auto-merge.").into());}
        }
        let view: Element<'_,Action> = widget::column::with_children(rows).spacing(space::S).width(Length::Fill).into();
        view.map(|action|self.fenced(super::Action::Completion(action)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull_request_review::tests::{entry, loaded, remote_file, state, target};
    #[test]
    fn dismissed_marks_are_distinct_counted_filterable_and_survive_refusal() {
        let (mut state,_) = state();
        state.target=Some(target(1));
        let mut changed=remote_file("src/changed.rs",false);changed.viewer_viewed_state=FileViewedState::Dismissed;
        state.adopt(&loaded(vec![entry("src/changed.rs",ChangeKind::Modified),entry("src/new.rs",ChangeKind::Modified)],vec![changed,remote_file("src/new.rs",false)]));
        assert!(state.files[0].changed_since_viewed);assert!(!state.files[0].viewed);assert!(!state.files[1].changed_since_viewed);
        drop(state.update(super::super::Action::ToggleChangedSinceViewed));
        let visible=state.displayed_file_rows(&directory_tallies(&state.rows,&state.files));
        assert!(visible.iter().any(|index|state.rows[*index].path.display()=="src/changed.rs"));
        assert!(!visible.iter().any(|index|state.rows[*index].path.display()=="src/new.rs"));
        let generation=state.generation;let authority_revision=state.authority_revision;
        drop(state.set_viewed(vec![0],true));assert!(state.files[0].changed_since_viewed);
        drop(state.update(super::super::Action::ViewedApplied{generation,authority_revision,results:vec![(state.files[0].entry.path.clone(),true,ViewedOutcome::NotApplied("denied".into()))]}));
        assert!(state.files[0].changed_since_viewed);assert!(!state.files[0].viewed);
        drop(state.update(super::super::Action::ViewedApplied{generation,authority_revision,results:vec![(state.files[0].entry.path.clone(),true,ViewedOutcome::Applied)]}));
        assert!(!state.files[0].changed_since_viewed);
    }
    #[test]
    fn uncertain_control_write_blocks_navigation_until_successful_reconciliation() {
        let (mut state,_)=state();state.target=Some(target(1));state.completion.uncertain=true;
        assert!(state.completion.blocker().is_some());
        drop(state.update_completion(Action::Reload));assert!(state.completion.uncertain,"starting a read cannot prove the prior write's outcome");
        let generation=state.generation;
        drop(state.update_completion(Action::Loaded{generation,request:state.completion.load_id,result:Err("offline".into())}));assert!(state.completion.uncertain);
    }
    #[test]
    fn stale_control_receipt_cannot_clear_a_newer_write() {
        let (mut state,_)=state();state.completion.busy=true;
        drop(state.update_completion(Action::Finished{generation:state.generation.wrapping_add(1),result:Ok(ReviewMutationOutcome::Applied(()))}));
        assert!(state.completion.busy);
    }
}
