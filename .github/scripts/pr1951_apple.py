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

def exact(label, selector, feature=None):
    command = ['cargo', 'test', '--locked', '--no-default-features']
    if feature:
        command += ['--features', feature]
    command += ['--lib', selector, '--', '--exact', '--show-output', '--test-threads=1', '--color=never']
    output = phase(label, command)
    assert len(re.findall(r'^test ' + re.escape(selector) + r' \.\.\. ok$', output, re.M)) == 1, selector
    assert re.findall(r'^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;', output, re.M) == [('1', '0', '0')], selector
    return output

phase('apple-cli-check', ['cargo','check','--locked','--no-default-features','--bin','cagents'])
build = phase('apple-cli-build', ['cargo','build','--locked','--no-default-features','--bin','cagents','--message-format=json-render-diagnostics'])
artifacts=[]
for line in build.splitlines():
    try:
        item=json.loads(line)
    except ValueError:
        continue
    if item.get('reason')=='compiler-artifact' and item.get('target',{}).get('name')=='cagents' and item.get('executable'):
        assert item.get('features')==[], item
        artifacts.append(item)
assert len(artifacts)==1, artifacts
binary=pathlib.Path(artifacts[0]['executable']).resolve(strict=True)
assert binary.is_relative_to(pathlib.Path(os.environ['CARGO_TARGET_DIR']).resolve(strict=True))
binary_hash=hashlib.sha256(binary.read_bytes()).hexdigest()
(logs/'linked-cli.json').write_text(json.dumps(dict(path=str(binary),sha256=binary_hash,artifact=artifacts[0]),indent=2)+'\n')
output=phase('apple-cli-protocol-version', [str(binary),'--json','--device','unconfigured-portability-device','daemon','protocol-version'])
reply=json.loads(output)
assert reply['ok'] is True and reply['command']=='daemon.protocol_version' and reply['data']['protocol_version']==127, reply
assert hashlib.sha256(binary.read_bytes()).hexdigest()==binary_hash
phase('apple-lib-test-compile',['cargo','test','--locked','--no-default-features','--lib','--no-run'])
for index, selector in enumerate(['update::tests::unsupported_streaming_never_starts_a_command_and_keeps_cancellation', 'install_guard::tests::unsupported_installation_refuses_before_path_or_window_preparation', 'instance::tests::unsupported_process_census_never_reports_an_empty_owned_inventory', 'update::source::platform_tests::unsupported_source_cannot_be_classified_as_an_absent_daemon', 'daemon::unsupported_platform_tests::ownership_refusal_precedes_runtime_discovery_and_socket_or_lease_access', 'daemon::unsupported_platform_tests::unsupported_descriptor_sealing_preserves_errno_without_executing_child', 'control::unsupported_connection_tests::unsupported_control_connection_precedes_path_validation_and_deadline_arithmetic', 'daemon::holder_registry::connection::unsupported_tests::unsupported_holder_connection_precedes_deadline_and_endpoint_work', 'fs_safety::tests::metadata_beneath_describes_a_symlink_as_itself', 'fs_safety::tests::a_symlinked_intermediate_component_is_rejected', 'fs_safety::tests::retained_directory_listing_and_permissions_ignore_replaced_names', 'fs_safety::tests::creation_modes_preserve_target_width_before_creating_entries', 'fs_safety::tests::unavailable_anonymous_publication_leaves_no_named_fallback', 'fs_safety::platform::tests::native_access_acl_copy_replaces_existing_acl_and_removes_observed_absence', 'fs_safety::tests::a_fifo_opens_without_blocking_and_is_identifiable', 'fs_safety::tests::exclusive_publication_preserves_existing_entries_and_dangling_links', 'worktree_cleanup::tests::root_operation_protection_uses_native_event_only_directory_pins'], 1):
    exact("apple-parent-%02d" % index, selector)
print('Seventeen native owning CLI parents passed exactly once.', flush=True)
owner_module.assert_sequence_success()
