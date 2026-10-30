import hashlib,json,os,pathlib,re,subprocess,time
research=pathlib.Path(os.environ['GITHUB_WORKSPACE'])/'research'
raw=(research/'.github/scripts/pr1951_phase.py').read_bytes()
assert hashlib.sha256(raw).hexdigest()=='0d404a70729e399836103d34f299cd22b67b91f24ac853a09f77c0bdcb523aea'
import types
owner_module=types.ModuleType('pr1951_phase')
exec(compile(raw,str(research/'.github/scripts/pr1951_phase.py'),'exec'),owner_module.__dict__)
logs=pathlib.Path(os.environ['EVIDENCE_DIR']);records=[]
def phase(label,command):
    text,record=owner_module.run_owned_phase(label,command,logs,2400)
    records.append(record)
    (logs/'phases.json').write_text(json.dumps(records,indent=2)+'\n')
    return text

def require_parents(text,names):
 for name in names:
  count=len(re.findall(r'^test '+re.escape(name)+r' \.\.\. ok$',text,re.M))
  assert count==2 if name=='fs_safety::directory_creation::tests::restrictive_umask_creation_uses_retained_descriptor' else count==1,(name,count)

def owning_harnesses(label,text):
 headers=list(re.finditer(r'^ +Running (?:unittests )?([^\n]+?) \([^\n]+\)$',text,re.M))
 assert headers,'No owning Cargo harnesses recorded'
 sections={};receipts=[]
 for index,header in enumerate(headers):
  name=header.group(1)
  assert name not in sections,('duplicate owning harness',name)
  body=text[header.end():headers[index+1].start() if index+1<len(headers) else len(text)]
  starts=re.findall(r'^running (\d+) tests?$',body,re.M)
  results=re.findall(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;',body,re.M)
  assert starts and results,('missing owning result',name)
  status,p,f,i,m,filtered=results[-1];p,f,i,m,filtered=map(int,[p,f,i,m,filtered])
  assert status=='ok' and f==m==filtered==0 and p+i==int(starts[0]),(name,results[-1],starts[0])
  cfg_empty=set(['src/main.rs','src/bin/cagents-ui.rs'])
  if label=='cli':cfg_empty.update(['tests/repository_git_contract.rs', 'tests/session_restore_contract.rs', 'tests/iced_tree_reconciliation.rs', 'tests/native_accessibility_button_routing.rs', 'tests/browser_capability_contract.rs', 'tests/terminal_metadata_contract.rs', 'tests/terminal_load_contract.rs', 'tests/upgrade_inspection_contract.rs', 'tests/codex_continuity_qualification.rs', 'tests/automation_precheck_contract.rs', 'tests/agent_console_contract.rs', 'tests/automation_cli_contract.rs', 'tests/account_default_contract.rs', 'tests/repository_scale_contract.rs', 'tests/account_cache_contract.rs'])
  if label=='host':cfg_empty.update(['tests/repository_git_contract.rs', 'tests/session_restore_contract.rs', 'tests/iced_tree_reconciliation.rs', 'tests/native_accessibility_button_routing.rs', 'tests/browser_capability_contract.rs', 'examples/terminal-workload.rs'])
  if label=='ui':cfg_empty.update(['tests/terminal_metadata_contract.rs', 'tests/terminal_load_contract.rs', 'tests/upgrade_inspection_contract.rs', 'tests/codex_continuity_qualification.rs', 'tests/automation_precheck_contract.rs', 'tests/agent_console_contract.rs', 'tests/automation_cli_contract.rs', 'tests/account_default_contract.rs', 'examples/repair-closed-terminal-owner.rs', 'examples/enrich-shared-terminal-hint.rs'])
  ignored_only={'tests/codex_continuity_qualification.rs':3} if label=='host' else {}
  if p+i==0:
   assert name in cfg_empty,('unqualified empty owner',label,name)
   owner_state='known cfg-empty; zero passes'
  elif p==0:
   assert ignored_only.get(name)==i,('unqualified ignored-only owner',label,name,i)
   owner_state='known live-provider ignored-only; zero passes'
  else:owner_state='actual nonempty owning harness'
  sections[name]=body;receipts.append(dict(target=name,passed=p,failed=f,ignored=i,measured=m,filtered=filtered,discovered=int(starts[0]),nested_summaries=len(results)-1,owner_state=owner_state))
 (logs/(label+'-owning-harnesses.json')).write_text(json.dumps(receipts,indent=2)+'\n')
 assert 'src/lib.rs' in sections
 return sections,receipts

def freeze_helper(source, artifact, label):
 import stat
 source=pathlib.Path(source).resolve(strict=True)
 target=pathlib.Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True)
 assert source.is_relative_to(target),source
 directory=pathlib.Path(os.environ['RUNNER_TEMP'])/(label+'-helper-'+os.environ['EXPECTED_PRODUCT_SHA'])
 assert not directory.is_relative_to(target)
 directory.mkdir(mode=0o700)
 frozen=directory/'cagentsd'
 source_fd=os.open(source,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC)
 digest=hashlib.sha256()
 with os.fdopen(source_fd,'rb') as original:
  before=os.fstat(original.fileno())
  assert stat.S_ISREG(before.st_mode) and before.st_uid==os.geteuid() and before.st_mode&0o111
  frozen_fd=os.open(frozen,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_CLOEXEC,0o600)
  with os.fdopen(frozen_fd,'wb') as output:
   while True:
    block=original.read(1024*1024)
    if not block:break
    digest.update(block);output.write(block)
   output.flush();os.fchmod(output.fileno(),0o500);os.fsync(output.fileno())
  after=os.fstat(original.fileno())
  assert (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)==(after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns)
 copied=frozen.stat();assert stat.S_ISREG(copied.st_mode) and copied.st_uid==os.geteuid() and copied.st_nlink==1
 assert stat.S_IMODE(copied.st_mode)==0o500 and (copied.st_dev,copied.st_ino)!=(before.st_dev,before.st_ino)
 copied_digest=hashlib.sha256()
 with frozen.open('rb') as output:
  for block in iter(lambda:output.read(1024*1024),b''):copied_digest.update(block)
 helper_hash=digest.hexdigest();assert copied_digest.hexdigest()==helper_hash
 record=dict(head=os.environ['EXPECTED_PRODUCT_SHA'],tree=os.environ['EXPECTED_PRODUCT_TREE'],
  build_command=['cargo','build','--locked','--no-default-features','--features','host','--bin','cagentsd','--message-format=json-render-diagnostics'],
  artifact=artifact,source=dict(path=str(source),sha256=helper_hash,device=before.st_dev,inode=before.st_ino,nlink=before.st_nlink),
  frozen=dict(path=str(frozen),sha256=helper_hash,device=copied.st_dev,inode=copied.st_ino,uid=copied.st_uid,type='regular',nlink=copied.st_nlink,mode=oct(stat.S_IMODE(copied.st_mode)),size=copied.st_size,mtime_ns=copied.st_mtime_ns,ctime_ns=copied.st_ctime_ns))
 (logs/'frozen-helper.json').write_text(json.dumps(record,indent=2)+'\n')
 with open(os.environ['GITHUB_OUTPUT'],'a') as outputs:
  print('frozen_helper='+json.dumps(record['frozen'],separators=(',',':')),file=outputs)
 return frozen,helper_hash,dict(record['frozen'])

import types
source_raw=(research/'.github/scripts/pr1951_source.py').read_bytes()
source_module=types.ModuleType('pr1951_source')
exec(compile(source_raw,str(research/'.github/scripts/pr1951_source.py'),'exec'),source_module.__dict__)
def helper_observation(label):
 return source_module.helper_observation(logs,label,helper_expected,required=True)

phase('format',['scripts/check-format'])
phase('cli-check',['cargo','check','--locked','--no-default-features','--bin','cagents'])
cli=phase('cli-all-targets',['cargo','test','--locked','--no-default-features','--all-targets','--','--test-threads=2','--show-output','--color=never'])
cli_sections,cli_receipts=owning_harnesses('cli',cli)
cli_required=['fs_safety::tests::a_root_reports_its_own_identity', 'fs_safety::tests::a_relative_path_resolves_and_reads', 'fs_safety::tests::a_symlinked_final_component_is_rejected', 'fs_safety::tests::a_symlinked_intermediate_component_is_rejected', 'fs_safety::tests::an_absolute_path_and_a_parent_component_are_rejected', 'fs_safety::tests::metadata_beneath_describes_a_symlink_as_itself', 'fs_safety::tests::exclusive_publication_preserves_existing_entries_and_dangling_links', 'fs_safety::tests::retained_directory_listing_and_permissions_ignore_replaced_names', 'fs_safety::tests::creation_modes_preserve_target_width_before_creating_entries', 'fs_safety::tests::a_temporary_file_is_renamed_over_its_target_atomically', 'fs_safety::tests::only_the_user_and_access_control_namespaces_are_reproduced', 'fs_safety::tests::a_fifo_opens_without_blocking_and_is_identifiable', 'fs_safety::directory_creation::tests::publication_retains_original_before_public_target_replacement', 'fs_safety::directory_creation::tests::different_parent_preserves_native_creation_as_unverified', 'fs_safety::directory_creation::tests::shared_namespace_returns_unverified_without_permission_claim', 'fs_safety::directory_creation::tests::restrictive_umask_creation_uses_retained_descriptor', 'fs_safety::directory_creation::tests::mode_restore_failure_keeps_published_identity_and_explicit_retry', 'fs_safety::directory_creation::tests::pending_mode_retry_preserves_observed_newer_public_permissions', 'fs_safety::directory_creation::tests::sticky_parent_effective_access_checks_the_retained_original_before_creation', 'fs_safety::directory_creation::tests::postpublication_cleanup_error_retains_cached_identity_and_stage', 'fs_safety::directory_creation::tests::failed_publication_retains_prepared_identity_and_retries_cleanup', 'update::tests::streaming_capture_failure_cleans_fresh_original_before_returning_error', 'update::tests::bounded_stream_preserves_success_and_size_failure', 'update::tests::bounded_stream_keeps_live_timeout_and_cancellation_cleanup', 'update::tests::streaming_child_observation_keeps_leader_until_final_cleanup', 'update::tests::streaming_signal_error_reaps_exited_leader_and_refuses_live_wait', 'update::tests::bounded_failed_stream_cleans_owned_descendants_after_exit', 'update::tests::a_failed_build_requires_a_later_matched_installation_to_be_superseded', 'control::agents::tests::accepted_wait_recovery_keeps_device_routing_and_original_error_details', 'fs_safety::tests::metadata_observation_matches_native_fields_for_all_entry_types', 'fs_safety::tests::metadata_observation_stays_with_retained_root_and_clone', 'accepted_wait_errors_retain_original_receipts_without_resubmission', 'accepted_final_status_errors_retain_original_receipts_without_resubmission', 'accepted_wait_recovery_is_readable_and_normal_timeout_stays_distinct']
require_parents(cli_sections['src/lib.rs'],cli_required[:-3])
require_parents(cli_sections['tests/cli_list_contracts.rs'],cli_required[-3:])
required_cli_targets=['tests/ceditor_install_contract.rs','tests/cli_help_contracts.rs','tests/cli_image_asset_contracts.rs','tests/cli_list_contracts.rs','tests/cli_read_contracts.rs','tests/installer_guard_contract.rs','tests/installer_lifecycle_contract.rs','tests/latency_disabled.rs','tests/linear_cli_contract.rs','tests/ui_launch_handshake_contract.rs','examples/replace-installed-daemon.rs']
for target in required_cli_targets:
 assert target in cli_sections and next(item['passed'] for item in cli_receipts if item['target']==target)>0,target
assert next((item['passed'],item['ignored']) for item in cli_receipts if item['target']=='tests/cli_list_contracts.rs')==(7,0)

assert re.search(r'^test update::tests::streaming_child_refuses_a_kernel_reused_original_pid \.\.\. ignored',cli,re.M)
assert not re.search(r'^test update::tests::streaming_child_refuses_a_kernel_reused_original_pid \.\.\. ok$',cli,re.M)
phase('host-check',['cargo','check','--locked','--no-default-features','--features','host','--bin','cagentsd'])
output=phase('matched-daemon-build',['cargo','build','--locked','--no-default-features','--features','host','--bin','cagentsd','--message-format=json-render-diagnostics'])
artifacts=[]
for line in output.splitlines():
 try:item=json.loads(line)
 except ValueError:continue
 if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='cagentsd' and item.get('executable'):artifacts.append(item)
assert len(artifacts)==1 and artifacts[0]['features']==['host'],artifacts
helper,helper_hash,helper_expected=freeze_helper(artifacts[0]['executable'],artifacts[0],'pr1951-full')
os.environ['CAGENTS_TEST_RETAINED_ACTOR_HELPER']=str(helper)
helper_observation('before-host')
try:
 host=phase('host-all-targets',['cargo','test','--locked','--no-default-features','--features','host','--all-targets','--','--test-threads=2','--show-output','--color=never'])
finally:
 helper_observation('after-host')
host_sections,host_receipts=owning_harnesses('host',host)
require_parents(host_sections['src/lib.rs'],['worktree_setup::tests::anonymous_copy_publishes_the_owned_inode_and_never_cleans_foreign_names', 'worktree_setup::tests::anonymous_copy_preserves_mode_under_restrictive_umask', 'worktree_setup::tests::changed_source_cannot_publish_new_private_bytes_with_old_permissions', 'worktree_setup::tests::source_changes_after_permission_application_refuse_publication', 'worktree_setup::tests::copy_preserves_restrictive_access_acl_and_removes_inherited_acl', 'chat::native_holder::transport::tests::post_child_census_failure_preserves_live_ownership_and_services_control', 'chat::native_holder::transport::tests::idle_reaping_preserves_a_live_provider_and_its_live_adopted_tool', 'git_watch::tests::native_watcher_observes_atomic_replacement_and_recreated_directory', 'fs_safety::tests::metadata_observation_matches_native_fields_for_all_entry_types', 'fs_safety::tests::metadata_observation_stays_with_retained_root_and_clone'])
require_parents(host_sections['src/lib.rs'],json.loads((research/'.github/scripts/pr1951-product-source.json').read_text())['fixture_parents'])
require_parents(host_sections['src/lib.rs'],json.loads((research/'.github/scripts/pr1951-product-source.json').read_text())['required_host_additional'])
assert 'SETUP_ACL_COPY_VERIFIED' in host and 'SETUP_ACL_FIXTURE_UNSUPPORTED' not in host
phase('ui-check',['cargo','check','--locked','--no-default-features','--features','ui','--bin','cagents-ui'])
helper_observation('before-ui')
try:
 ui=phase('ui-all-targets',['cargo','test','--locked','--no-default-features','--features','ui','--all-targets','--','--test-threads=2','--show-output','--color=never'])
finally:
 helper_observation('after-ui')
ui_sections,ui_receipts=owning_harnesses('ui',ui)
require_parents(ui_sections['src/lib.rs'],['fs_safety::tests::editor_copy_preserves_access_acl_and_removes_unwanted_inheritance', 'fs_safety::tests::metadata_observation_matches_native_fields_for_all_entry_types', 'fs_safety::tests::metadata_observation_stays_with_retained_root_and_clone'])
assert 'EDITOR_ACL_COPY_VERIFIED' in ui and 'EDITOR_ACL_FIXTURE_UNSUPPORTED' not in ui
print('All three isolated all-target boundaries passed at the literal product head.',flush=True)
owner_module.assert_sequence_success()
