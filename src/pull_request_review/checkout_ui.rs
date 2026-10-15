//! An existing checkout is offered, reconciled on a worker, then opened from
//! a verified receipt. Merely finding a similarly named branch grants nothing.
use super::*;
use crate::repository::{ExistingPrCheckoutPlan, PrCheckoutDisposition, VerifiedPrCheckout};

#[derive(Debug, Clone)]
pub enum Action {
    Confirm,
    Cancel,
    Open,
    Finished { generation: u64, ticket: u64, result: Result<VerifiedPrCheckout, String> },
}

#[derive(Default)]
pub(super) struct Confirmation {
    ticket: u64,
    target: Option<CheckoutTarget>,
    plan: Option<ExistingPrCheckoutPlan>,
    notice: Option<String>,
    cancellation: Option<CancellationToken>,
    completed: Option<VerifiedPrCheckout>,
    error: Option<String>,
    accepted: bool,
}
impl Confirmation {
    pub(super) fn busy(&self)->bool {self.cancellation.is_some()}
    pub(super) fn reset(&mut self) {
        if let Some(cancel)=self.cancellation.take() { cancel.cancel(); }
        self.ticket=self.ticket.wrapping_add(1);
        self.target=None;self.plan=None;self.notice=None;self.completed=None;self.error=None;self.accepted=false;
    }
}

impl State {
    #[cfg(test)]
    pub(crate) fn fixture_existing_checkout_pending(&mut self)->u64 {
        self.existing_checkout.ticket=self.existing_checkout.ticket.wrapping_add(1);
        self.existing_checkout.accepted=true;
        self.existing_checkout.cancellation=Some(CancellationToken::new());
        self.existing_checkout.ticket
    }

    fn existing_checkout_target_is_current(&self, target:&CheckoutTarget)->bool {
        self.range.as_ref().is_some_and(|range|range.head==target.head)
            && self.target.as_ref().is_some_and(|review|
                review.worktree==target.repository && review.pull_request.number==target.number
                    && target.pull_request.as_ref()==PullRequestKey::new(review.repository.clone(),target.number).ok().as_ref())
    }

    pub fn offer_existing_checkout(&mut self,target:CheckoutTarget,plan:ExistingPrCheckoutPlan,notice:String) {
        if self.existing_checkout.busy() || !self.existing_checkout_target_is_current(&target) || plan.request().expected_head!=target.head {return;}
        self.existing_checkout.reset();
        self.existing_checkout.target=Some(target);
        self.existing_checkout.plan=Some(plan);
        self.existing_checkout.notice=Some(notice);
    }

    pub(super) fn update_existing_checkout(&mut self,action:Action)->ActionTask {
        match action {
            Action::Cancel=>{
                if let Some(cancel)=&self.existing_checkout.cancellation {
                    cancel.cancel();
                    self.existing_checkout.error=Some("Cancellation requested; waiting for the checkout outcome.".into());
                } else {self.existing_checkout.reset();}
            }
            Action::Confirm=>{
                let Some(target)=self.existing_checkout.target.as_ref() else{return ActionTask::none()};
                if self.refreshing || self.existing_checkout.cancellation.is_some()
                    || self.existing_checkout.accepted
                    || !self.existing_checkout_target_is_current(target) {return ActionTask::none()}
                let Some(plan)=self.existing_checkout.plan.clone() else{return ActionTask::none()};
                let cancellation=CancellationToken::new();
                self.existing_checkout.cancellation=Some(cancellation.clone());
                self.existing_checkout.completed=None;self.existing_checkout.error=None;
                self.existing_checkout.accepted=true;
                self.existing_checkout.ticket=self.existing_checkout.ticket.wrapping_add(1);
                let ticket=self.existing_checkout.ticket;let generation=self.generation;
                return ActionTask::perform(async move {
                    let result=tokio::task::spawn_blocking(move ||RepositoryService::system("git")
                        .reconcile_existing_pr_checkout(&plan,&cancellation).map_err(|error|error.to_string()))
                        .await.map_err(|error|error.to_string()).and_then(|result|result);
                    Action::Finished{generation,ticket,result}
                },|action|super::Action::ExistingCheckout(action));
            }
            Action::Finished{generation,ticket,result}=>{
                if generation!=self.generation || ticket!=self.existing_checkout.ticket {return ActionTask::none()}
                let cancelled=self.existing_checkout.cancellation.take().is_some_and(|token|token.is_cancelled());
                match result {
                    Ok(receipt)=>{
                        self.existing_checkout.completed=Some(receipt);
                        self.existing_checkout.error=cancelled.then(||"Checkout reconciliation completed after cancellation was requested. Review the outcome before opening it.".into());
                    },
                    Err(error)=>self.existing_checkout.error=Some(error),
                }
            }
            Action::Open=>{
                let (Some(target),Some(plan),Some(receipt))=(&self.existing_checkout.target,&self.existing_checkout.plan,&self.existing_checkout.completed) else{return ActionTask::none()};
                if self.refreshing || !self.existing_checkout_target_is_current(target) {
                    self.existing_checkout.error=Some("The checkout no longer matches this review. Prepare checkout again.".into());
                    return ActionTask::none();
                }
                let path=match checkout::verified_existing_path(target,plan,receipt) {
                    Ok(path)=>path,Err(error)=>{self.existing_checkout.error=Some(error);return ActionTask::none()}
                };
                self.existing_checkout.reset();
                self.effects.push(Effect::OpenCheckout(path));
            }
        }
        ActionTask::none()
    }

    pub(super) fn existing_checkout_view(&self,modal_open:bool)->Option<Element<'_,super::Action>> {
        let plan=self.existing_checkout.plan.as_ref()?;
        let busy=self.existing_checkout.cancellation.is_some();
        let current=self.existing_checkout.target.as_ref().is_some_and(|target|self.existing_checkout_target_is_current(target));
        let action=|action|self.fenced(super::Action::ExistingCheckout(action));
        let mut rows=vec![design::caption(self.existing_checkout.notice.as_deref().unwrap_or_else(||plan.notice())).into()];
        if !current {rows.push(design::caption("The PR head changed. Prepare checkout again.").into());}
        if let Some(error)=&self.existing_checkout.error {rows.push(design::caption(error).into());}
        if self.existing_checkout.accepted&&!busy&&self.existing_checkout.completed.is_none() {
            rows.push(design::caption("Inspect this outcome, then prepare checkout again before retrying.").into());
        }
        let primary=if self.existing_checkout.completed.is_some() {
            crate::ui_controls::button::secondary("Open existing worktree")
                .on_press_maybe((!modal_open&&!busy&&current&&!self.refreshing).then(||action(Action::Open)))
        } else {
            let label=if busy {"Reconciling checkout…"} else {match plan.disposition() {
                PrCheckoutDisposition::AlreadyCurrent=>"Verify existing checkout",
                PrCheckoutDisposition::FastForward=>"Update checkout",
                PrCheckoutDisposition::RepairTracking=>"Set PR upstream",
                PrCheckoutDisposition::FastForwardAndRepair=>"Update checkout and set PR upstream",
            }};
            crate::ui_controls::button::secondary(label)
                .on_press_maybe((!modal_open&&!busy&&!self.refreshing&&current&&!self.existing_checkout.accepted).then(||action(Action::Confirm)))
        };
        rows.push(widget::row::with_children(vec![primary.into(),
            crate::ui_controls::button::quiet(if busy {"Cancel reconciliation"} else {"Dismiss"})
                .on_press_maybe((!modal_open).then(||action(Action::Cancel))).into()])
            .spacing(space::S).into());
        Some(widget::column::with_children(rows).spacing(space::S).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{ExistingPrCheckoutRequest,PrCheckoutTracking,ValidatedRefName};
    use futures_util::StreamExt as _;

    fn git(directory:&Path,args:&[&str])->String {
        let output=std::process::Command::new("git").arg("-C").arg(directory)
            .args(["-c","core.hooksPath=/dev/null","-c","core.fsmonitor=false"])
            .args(args).env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_SYSTEM","/dev/null")
            .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR").output().unwrap();
        assert!(output.status.success(),"Git {args:?}: {}",String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn existing_checkout_tracking_changes_only_after_confirmation_and_opens_only_after_verified_receipt() {
        let directory=tempfile::tempdir().unwrap();
        let repository=directory.path().join("repository");let remote=directory.path().join("remote.git");
        let worktree=directory.path().join("existing");
        git(directory.path(),&["init","--bare",remote.to_str().unwrap()]);
        git(directory.path(),&["init","--initial-branch=main",repository.to_str().unwrap()]);
        git(&repository,&["config","user.name","Fixture"]);
        git(&repository,&["config","user.email","fixture@example.invalid"]);
        std::fs::write(repository.join("README"),"Reviewed content\n").unwrap();
        git(&repository,&["add","README"]);git(&repository,&["commit","-m","Initial fixture"]);
        git(&repository,&["push",remote.to_str().unwrap(),"HEAD:refs/heads/feature"]);
        git(&repository,&["worktree","add","--no-track","-b","feature",worktree.to_str().unwrap(),"HEAD"]);
        let url="https://fixture.invalid/owner/repo.git";
        git(&repository,&["remote","add","origin",url]);
        git(&repository,&["config",&format!("url.{}.insteadOf",remote.display()),url]);
        git(&repository,&["config","fixture.keep","unrelated configuration"]);
        let head=Oid::parse(&git(&repository,&["rev-parse","HEAD"])).unwrap();
        let key=GitHubRepoKey::new("fixture.invalid",None,"owner","repo").unwrap();
        let request=ExistingPrCheckoutRequest {
            repository:repository.clone(),worktree:worktree.clone(),branch:ValidatedBranchName::parse("feature").unwrap(),
            head_repository:key.clone(),head_branch:ValidatedBranchName::parse("feature").unwrap(),
            expected_head:head.clone(),remote:"origin".into(),tracking:PrCheckoutTracking::RemoteBranch,
        };
        let config=std::fs::read(repository.join(".git/config")).unwrap();
        let plan=RepositoryService::system("git").prepare_existing_pr_checkout(&request,&CancellationToken::new()).unwrap();
        assert_eq!(plan.disposition(),PrCheckoutDisposition::RepairTracking);
        let target=CheckoutTarget {repository:repository.clone(),number:7,head:head.clone(),
            pull_request:Some(PullRequestKey::new(key.clone(),7).unwrap()),head_ref_name:"feature".into()};
        let pr:PullRequest=serde_json::from_value(serde_json::json!({
            "number":7,"title":"Fixture","state":"OPEN","headRefName":"feature","headRefOid":head.as_str(),
            "baseRefName":"main","updatedAt":"2026-10-15T00:00:00Z","url":"https://fixture.invalid/owner/repo/pull/7"
        })).unwrap();
        let mut state=State::new(Arc::new(crate::github::GitHubService::system("gh")),RepositoryService::system("git"));
        state.target=Some(ReviewTarget {repository:key,worktree:repository.clone(),pull_request:Arc::new(pr)});
        state.range=Some(crate::repository::PullRequestRange {
            number:7,base_branch:ValidatedBranchName::parse("main").unwrap(),
            head_mirror:ValidatedRefName::parse("refs/cagents/fixture/head").unwrap(),
            base_mirror:ValidatedRefName::parse("refs/cagents/fixture/base").unwrap(),
            head:head.clone(),base_tip:head.clone(),merge_base:head.clone(),
        });
        state.load=LoadState::Ready;
        state.offer_existing_checkout(target,plan,"Set the existing PR branch upstream.".into());
        assert_eq!(std::fs::read(repository.join(".git/config")).unwrap(),config);
        assert!(git(&repository,&["for-each-ref","--format=%(upstream)","refs/heads/feature"]).is_empty());
        assert!(state.take_effects().is_empty(),"offering reconciliation does not open an untracked checkout");
        let task=state.update(super::super::Action::ExistingCheckout(Action::Confirm));
        assert!(state.restart_blocker().is_some());assert!(state.take_effects().is_empty());
        let reply=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let mut stream=iced::runtime::task::into_stream(task).unwrap();
            while let Some(action)=stream.next().await {
                if let iced::runtime::Action::Output(action @ super::super::Action::ExistingCheckout(Action::Finished {..}))=action {return action;}
            }
            panic!("confirmed checkout produced no receipt");
        });
        drop(state.update(reply));
        assert!(state.existing_checkout.completed.is_some(),"{:?}",state.existing_checkout.error);
        assert_eq!(git(&repository,&["for-each-ref","--format=%(upstream)","refs/heads/feature"]),"refs/remotes/origin/feature");
        assert_eq!(git(&repository,&["config","fixture.keep"]),"unrelated configuration");
        assert!(state.take_effects().is_empty(),"the user decides when to open the verified result");
        state.range.as_mut().unwrap().head=Oid::parse(&"b".repeat(40)).unwrap();
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert!(state.take_effects().is_empty(),"a moved review cannot open the earlier receipt");
        state.range.as_mut().unwrap().head=head;
        drop(state.update(super::super::Action::ExistingCheckout(Action::Open)));
        assert_eq!(state.take_effects(),vec![Effect::OpenCheckout(worktree.canonicalize().unwrap())]);
        assert!(state.restart_blocker().is_none());
        std::fs::write(worktree.join("README"),"Reviewed content\nA local fix\n").unwrap();
        git(&worktree,&["add","README"]);git(&worktree,&["commit","-m","Fix after checkout"]);
        git(&worktree,&["push"]);
        assert_eq!(git(&remote,&["rev-parse","refs/heads/feature"]),git(&worktree,&["rev-parse","HEAD"]),
            "ordinary git push must publish the existing checkout's fix to its PR head branch");
    }

    #[test]
    fn stale_checkout_receipts_and_cancel_requests_do_not_retire_an_accepted_new_operation() {
        let mut state=State::new(Arc::new(crate::github::GitHubService::system("gh")),RepositoryService::system("git"));
        let ticket=state.fixture_existing_checkout_pending();let generation=state.generation;
        drop(state.update(super::super::Action::ExistingCheckout(Action::Finished {
            generation,ticket:ticket.wrapping_sub(1),result:Err("older checkout".into()),
        })));
        assert!(state.existing_checkout.busy());assert!(state.restart_blocker().is_some());
        drop(state.update(super::super::Action::ExistingCheckout(Action::Cancel)));
        assert!(state.existing_checkout.cancellation.as_ref().unwrap().is_cancelled());
        assert!(state.restart_blocker().is_some(),"a cancellation request does not certify Git settlement");
        drop(state.update(super::super::Action::ExistingCheckout(Action::Finished {
            generation,ticket,result:Err("Current checkout preserved; reconciliation refused.".into()),
        })));
        assert!(!state.existing_checkout.busy());assert!(state.restart_blocker().is_none());
        assert!(state.existing_checkout.error.as_deref().unwrap().contains("preserved"));
        assert!(state.take_effects().is_empty(),"refused or stale receipts never open a worktree");
    }
}
