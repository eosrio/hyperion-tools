"""Pure namespace/process models and AST controls only; no execution-entry import."""
import ast
import base64
import copy
import importlib
from pathlib import Path,PurePosixPath
import unittest
from unittest import mock
import contracts as C
import namespace_contracts as N

HERE=Path(__file__).resolve().parent


def host():return {'pidNamespace':'pid:[10]','mountNamespace':'mnt:[11]','uid':1001,'gid':1001}
def snapshot(pid=2):
    return {'pid':pid,'ppid':0 if pid==1 else 1,'uid':1001,'gid':1001,'realUids':[1001]*3,'realGids':[1001]*3,
            'statusUids':[1001]*4,'statusGids':[1001]*4,'supplementaryGids':[],
            'pidNamespace':'pid:[20]','mountNamespace':'mnt:[21]','procSelf':str(pid),'initNamespace':'pid:[20]',
            'init':{'pid':1,'birth':100},'procMounted':True,'capEff':'0000000000000000','capPrm':'0','capInh':'0','capAmb':'0','noNewPrivs':'1'}
def row(pid,birth,ppid=1,session=1,state='S'):
    return {'pid':pid,'birth':birth,'ppid':ppid,'group':session,'session':session,'state':state,'comm':'modeled','pidNamespace':'pid:[20]'}
def stat_body(pid,birth,state='S'):
    fields=[state,'1','2','2']+['0']*15+[str(birth)]
    return str(pid)+' (modeled)x) '+' '.join(fields)


class NamespaceData(unittest.TestCase):
    def setUp(self):
        self.raw=(HERE/'profile-pidns.json').read_bytes();self.profile,self.members=C.load_contract(self.raw,(HERE/'npm-members.json').read_bytes())
    def test_v1_data_immutable_and_v2_explicit(self):
        self.assertEqual(C.pin((HERE/'profile.json').read_bytes()),{'bytes':4880,'sha256':'52a85b58b621ba9476b4cc0d5aaad1f35131cd9db10a4a1d91e31fff529f0f45'})
        self.assertEqual(self.profile['schemaVersion'],2);self.assertEqual(self.profile['runtime'],C.parse_json((HERE/'profile.json').read_bytes())['runtime'])
        self.assertTrue(all(v is None for v in self.profile['artifactPins'].values()))
    def test_complete_source_pins_and_no_runtime_substitution(self):
        N.source_pins(self.profile,HERE)
        bad=copy.deepcopy(self.profile);bad['executionContext']['sourceFiles']['namespace_init.py']['sha256']='0'*64
        with self.assertRaises(ValueError):N.source_pins(bad,HERE)
        bad=copy.deepcopy(self.profile);del bad['executionContext']['sourceFiles']['namespace_init.py']
        with self.assertRaises(ValueError):C.validate_profile(bad,self.members)
    def test_namespace_local_proc_and_unprivileged_role(self):
        N.validate_snapshot(snapshot(),host(),1001,1001);N.validate_snapshot(snapshot(1),host(),1001,1001,init=True)
        for key,value in [('pidNamespace','pid:[10]'),('mountNamespace','mnt:[11]'),('procSelf','999'),('procMounted',False),
                          ('initNamespace','pid:[30]'),('uid',0),('realUids',[0,1001,1001]),('capEff','1'),('noNewPrivs','0')]:
            bad=snapshot();bad[key]=value
            with self.subTest(key=key):
                with self.assertRaises(ValueError):N.validate_snapshot(bad,host(),1001,1001)
    def test_wrong_init_and_parent_role_refuse(self):
        bad=snapshot(1);bad['ppid']=42
        with self.assertRaises(ValueError):N.validate_snapshot(bad,host(),1001,1001,init=True)
        with self.assertRaises(ValueError):N.validate_snapshot(snapshot(),host(),1001,1001,init=True)
    def test_retained_permitted_inheritable_or_ambient_capabilities_refuse(self):
        for key in ('capEff','capPrm','capInh','capAmb'):
            for bits in ('1','0000000000000001','-0','0x0','',None):
                value=snapshot();value[key]=bits
                with self.subTest(key=key,bits=bits):
                    with self.assertRaises(ValueError):N.validate_snapshot(value,host(),1001,1001)
    def test_all_proc_uid_gid_values_and_supplementary_groups_are_unprivileged(self):
        for key in ('statusUids','statusGids'):
            for index in range(4):
                value=snapshot();value[key][index]=0
                with self.subTest(key=key,index=index):
                    with self.assertRaises(ValueError):N.validate_snapshot(value,host(),1001,1001)
        for uid,gid in ((0,1001),(1001,0),(True,1001),(1001,True)):
            with self.assertRaises(ValueError):N.validate_snapshot(snapshot(),host(),uid,gid)
        value=snapshot();value['supplementaryGids']=[0]
        with self.assertRaises(ValueError):N.validate_snapshot(value,host(),1001,1001)
    def test_snapshot_collects_all_proc_credentials_from_mocked_linux_reads(self):
        status='Uid:\t1001 1001 1001 1001\nGid:\t1001 1001 1001 1001\nGroups:\t\nCapEff:\t0\nCapPrm:\t0\nCapInh:\t0\nCapAmb:\t0\nNoNewPrivs:\t1\n'
        texts={'/proc/self/status':status,'/proc/1/stat':stat_body(1,100),'/proc/self/mountinfo':'1 2 0:1 / /proc rw - proc proc rw\n'}
        links={'/proc/self/ns/pid':'pid:[20]','/proc/self/ns/mnt':'mnt:[21]','/proc/self':'2','/proc/1/ns/pid':'pid:[20]'}
        calls={name:mock.Mock(return_value=value)for name,value in {'getpid':2,'getppid':1,'getuid':1001,'getgid':1001,'getresuid':(1001,)*3,'getresgid':(1001,)*3}.items()}
        with mock.patch.multiple(N.os,create=True,**calls),mock.patch.object(N.os,'readlink',side_effect=lambda path:links[path]),mock.patch.object(N,'read_text',side_effect=lambda path,*_:texts[path]):
            value=N.snapshot();N.validate_snapshot(value,host(),1001,1001)
            self.assertEqual(value['statusUids'],[1001]*4);self.assertEqual(value['statusGids'],[1001]*4)
            self.assertTrue(all(value[key]=='0'for key in ('capEff','capPrm','capInh','capAmb')))
    def test_shared_host_new_pid_predicate_remains_strict(self):
        baseline={1:row(1,10),2:row(2,20)};rows={**baseline,99:row(99,90)}
        _,live=N.classify(baseline,rows,set(),3)
        self.assertEqual([r['pid']for r in live],[99]);self.assertEqual(live[0]['reasons'],['new-identity-in-owned-namespace'])
        # Isolation removes host PIDs from the actual proc view; no name-based exception is added.
    def test_observed_and_reparented_escaped_descendants_block(self):
        before={1:row(1,10),2:row(2,20)};rows={**before,3:row(3,30,2,3),4:row(4,40,3,4)}
        observed,live=N.classify(before,rows,{(3,30)},3);self.assertIn((4,40),observed)
        _,live=N.classify(before,{**before,4:row(4,40,1,88)},observed,3)
        self.assertEqual(live[0]['pid'],4);self.assertIn('observed-command-descendant-or-session',live[0]['reasons'])
    def test_unobserved_escape_and_pid_reuse_still_block(self):
        before={1:row(1,10),5:row(5,50)}
        _,live=N.classify(before,{1:before[1],5:row(5,51,1,88)},set(),3)
        self.assertEqual(live[0]['birth'],51);self.assertIn('new-identity-in-owned-namespace',live[0]['reasons'])
    def test_zombie_is_not_silently_filtered(self):
        zombie=row(9,90,state='Z');zombie['pidNamespace']=None
        _,live=N.classify({}, {9:zombie},set(),3);self.assertEqual(live[0]['state'],'Z')
    def test_zombie_namespace_link_race_retains_stat(self):
        with mock.patch.object(N,'Path')as factory,mock.patch.object(N,'read_text',return_value=stat_body(9,90,'Z')),mock.patch.object(N.os,'readlink',side_effect=FileNotFoundError):
            factory.return_value.iterdir.return_value=[PurePosixPath('/proc/9')]
            rows=N.census();self.assertEqual(rows[9]['state'],'Z');self.assertIsNone(rows[9]['pidNamespace'])
    def test_unknown_permission_is_not_a_complete_census(self):
        with mock.patch.object(N,'Path')as factory,mock.patch.object(N,'read_text',return_value=stat_body(9,90)),mock.patch.object(N.os,'readlink',side_effect=PermissionError):
            factory.return_value.iterdir.return_value=[PurePosixPath('/proc/9')]
            with self.assertRaises(PermissionError):N.census()
    def test_pid_reuse_inside_one_census_refuses(self):
        with mock.patch.object(N,'Path')as factory,mock.patch.object(N,'read_text',side_effect=[stat_body(9,90),stat_body(9,91)]),mock.patch.object(N.os,'readlink',return_value='pid:[20]'):
            factory.return_value.iterdir.return_value=[PurePosixPath('/proc/9')]
            with self.assertRaisesRegex(ValueError,'PID reused'):N.census()
    def test_census_bound_fails_without_partial_success(self):
        with mock.patch.object(N,'Path')as factory,mock.patch.object(N,'read_text',side_effect=lambda p,*_:stat_body(int(p.parent.name),90)),mock.patch.object(N.os,'readlink',return_value='pid:[20]'):
            factory.return_value.iterdir.return_value=[PurePosixPath('/proc/'+str(p))for p in range(1,4098)]
            with self.assertRaisesRegex(ValueError,'census bound'):N.census()
    def test_command_identity_history_is_also_bounded(self):
        with self.assertRaisesRegex(ValueError,'identity history'):
            N.classify({}, {5000:row(5000,5000,1,5000)}, {(p,p)for p in range(1,4097)},5000)
    def test_stat_parser_binds_pid_and_birth(self):
        value=N.parse_stat(9,stat_body(9,90));self.assertEqual(value['comm'],'modeled)x');self.assertEqual(value['birth'],90)
        with self.assertRaises(ValueError):N.parse_stat(8,stat_body(9,90))
        with self.assertRaises(ValueError):N.parse_stat(9,'9 (short) S 1')
    def test_completion_never_uses_teardown_or_failed_producer(self):
        start={'host':host(),'uid':1001,'gid':1001,'init':snapshot(1)};pin=C.pin(self.raw);receipt=C.pin(b'receipt')
        value={'kind':N.KIND,'profile':pin,'producerExitCode':0,'producerReceipt':receipt,'producerReceiptValidated':True,
               'beforeTeardown':True,'leftoverProcesses':[],'scanErrors':[],'closureObservations':[{'live':[]},{'live':[]}],
               'reapCoverageComplete':True,'reapCount':1,'initFinal':snapshot(1)}
        N.validate_completion(value,pin,0,receipt,start)
        for key,bad in [('producerExitCode',1),('producerReceiptValidated',False),('beforeTeardown',False),('leftoverProcesses',[row(9,90)]),
                        ('scanErrors',['unknown']),('reapCoverageComplete',False),('closureObservations',[]),('reapCount',4097)]:
            changed=copy.deepcopy(value);changed[key]=bad
            with self.subTest(key=key):
                with self.assertRaises(ValueError):N.validate_completion(changed,pin,0,receipt,start)
    def test_completion_requires_same_original_init(self):
        start={'host':host(),'uid':1001,'gid':1001,'init':snapshot(1)};pin=C.pin(self.raw);receipt=C.pin(b'r')
        value={'kind':N.KIND,'profile':pin,'producerExitCode':0,'producerReceipt':receipt,'producerReceiptValidated':True,'beforeTeardown':True,
               'leftoverProcesses':[],'scanErrors':[],'closureObservations':[{'live':[]},{'live':[]}],'reapCoverageComplete':True,'reapCount':1,'initFinal':snapshot(1)}
        value['initFinal']['init']['birth']=101
        with self.assertRaises(ValueError):N.validate_completion(value,pin,0,receipt,start)
    def test_environment_retains_only_required_ci_and_path(self):
        values={'GITHUB_ACTIONS':'true','GITHUB_REPOSITORY':'eosrio/hyperion-tools','GITHUB_JOB':self.profile['job'],
                'GITHUB_WORKFLOW_REF':'eosrio/hyperion-tools/'+self.profile['workflow']+'@refs/heads/main','GITHUB_WORKFLOW_SHA':'a'*40,
                'GITHUB_RUN_ID':'1','GITHUB_RUN_ATTEMPT':'1','RUNNER_TRACKING_ID':'model-tracking','PATH':'/runtime/bin:/usr/bin',
                'NPM_TOKEN':'must-drop','NODE_OPTIONS':'must-drop','HOME':'must-drop'}
        result=N.selected_environment(values,self.profile);self.assertEqual(set(result),set(N.CI_KEYS))
        values['PATH']='relative:/usr/bin'
        with self.assertRaises(ValueError):N.selected_environment(values,self.profile)
    def test_entry_files_parse_without_import_process_or_namespace_effect(self):
        for name in ('namespace_init.py','namespace_launch.py','pidns_runner.py','producer.py'):
            tree=ast.parse((HERE/name).read_bytes(),filename=name)
            self.assertFalse(any(isinstance(n,ast.Attribute)and n.attr in ('kill','terminate','send_signal')for n in ast.walk(tree)))
        self.assertNotIn('namespace_init',globals());self.assertNotIn('namespace_launch',globals())
    def test_v1_is_manual_only_and_v2_is_the_only_automatic_producer(self):
        workflows=HERE.parents[1]/'.github/workflows'
        old=(workflows/'hapi-current103-packed-registry.yml').read_text();new=(workflows/'hapi-current103-packed-registry-pidns.yml').read_text()
        self.assertNotIn('pull_request:',old);self.assertIn('workflow_dispatch:',old)
        for text in ('pull_request:','namespace_launch.py','timeout-minutes: 45','cancel-in-progress: false','pidns-v2.context.json'):
            self.assertIn(text,new)
        self.assertNotIn('continue-on-error',new)
    def v2_receipt(self):
        model=importlib.import_module('test_contracts');proof=model.terminal();pin=C.pin(self.raw);start_pin=C.pin(b'modeled-context')
        start={'kind':N.KIND,'profile':pin,'host':host(),'uid':1001,'gid':1001,'init':snapshot(1),'initDumpable':1}
        stage={'contextPin':start_pin,'actual':snapshot()}
        layout={'productRoot':'/model/product','workRoot':'/model/work','toolsRoot':'/model/tools','node':'/runtime/bin/node','npm':'/runtime/lib/node_modules/npm/bin/npm-cli.js'}
        commands=[]
        for index,planned in enumerate(C.command_plan(layout,self.profile)):
            began=index*10;row=model.terminal();row.update(planned)
            row.update(startedMonotonic=began,closurePolicy=N.KIND,diagnosticErrors=[],namespaceBefore=copy.deepcopy(stage),namespaceAfter=copy.deepcopy(stage),
                       directExit={'at':began+0.3,'exitCode':0,'waitReaped':True},pipeEofTimes={'stdout':began+0.4,'stderr':began+0.45},
                       lastCompleteCensus={'complete':True,'live':[]},closureObservations=[{'complete':True,'live':[],'at':began+0.6},{'complete':True,'live':[],'at':began+0.8}])
            if index<4:
                body=(self.profile['runtime'][('node','npm','bun','typescript')[index]]+'\n').encode()
                row['stdout']={**C.pin(body),'base64':base64.b64encode(body).decode(),'eof':True}
            commands.append(row)
        proof=commands[-2]
        receipt={'schemaVersion':2,'kind':self.profile['kind'],'profile':pin,'product':self.profile['product'],'consumer':self.profile['consumer'],
                 'producer':{'repository':'eosrio/hyperion-tools','workflowPath':self.profile['workflow'],'job':self.profile['job'],
                             'workflowRevision':'a'*40,'workflowTree':'b'*40,'runId':1,'runAttempt':1},
                 'runtime':self.profile['runtime'],'offlineEnvironment':self.profile['offlineEnvironment'],
                 'scope':{'isolatedPackedConsumer':True,'fullWireComponent':'NOT_RUN','receiverLimitsUnchanged':self.profile['retainedReceiverLimits']},
                 'seedSealedBeforeProof':True,'seedContainsLocalPacksOrBuilds':False,'freshProofSourceAndBuild':True,'proof':proof,
                 'actualOutputPacks':[{'name':name,'role':'OUTPUT','origin':'actual-proof-mutable-cache-after-original-terminal'}for name in C.LOCAL_NAMES],
                 'postUploadArtifact':None,'commands':commands,'commandLayout':layout,
                 'executionContext':{'kind':N.KIND,'contextPin':start_pin,'start':start,'before':copy.deepcopy(stage),'after':copy.deepcopy(stage)}}
        return receipt
    def test_v2_producer_receipt_requires_original_context_and_every_command_proof(self):
        receipt=self.v2_receipt();pin=C.pin(self.raw)
        C.validate_receipt(receipt,self.profile,pin)
        for mutate in [lambda r:r['commands'][0]['lastCompleteCensus'].update(complete=False),
                       lambda r:r['commands'][0].update(diagnosticErrors=['capture failed']),
                       lambda r:r['executionContext']['start'].update(profile=C.pin(b'wrong'))]:
            bad=copy.deepcopy(receipt);mutate(bad)
            with self.assertRaises(ValueError):C.validate_receipt(bad,self.profile,pin)
        old=C.parse_json((HERE/'profile.json').read_bytes())
        with self.assertRaises(ValueError):C.validate_receipt(receipt,old,C.pin((HERE/'profile.json').read_bytes()))
    def test_exact_complete_command_sequence_and_caps_are_required(self):
        original=self.v2_receipt();self.assertEqual(len(original['commands']),24)
        mutations=[lambda r:r.update(commands=[]),lambda r:r['commands'].pop(),lambda r:r['commands'].pop(0),
                   lambda r:r['commands'].reverse(),lambda r:r['commands'].append(copy.deepcopy(r['commands'][-1])),
                   lambda r:r['commands'][4].update(limitSeconds=1201),lambda r:r['commands'][4]['argv'].append('--save-exact')]
        for mutate in mutations:
            value=copy.deepcopy(original);mutate(value)
            with self.assertRaises(ValueError):C.validate_receipt(value,self.profile,C.pin(self.raw))
    def test_receipt_checks_every_recorded_privilege_snapshot(self):
        original=self.v2_receipt()
        for key in ('capPrm','capInh','capAmb'):
            for target in ('init','before','after','command-before','command-after'):
                value=copy.deepcopy(original);context=value['executionContext']
                actual=context['start']['init'] if target=='init' else context[target]['actual'] if target in ('before','after') else value['commands'][0]['namespaceBefore' if target=='command-before' else 'namespaceAfter']['actual']
                actual[key]='1'
                with self.subTest(key=key,target=target):
                    with self.assertRaises(ValueError):C.validate_receipt(value,self.profile,C.pin(self.raw))
    def test_failed_setup_stream_or_direct_exit_cannot_hide_behind_empty_closure(self):
        original=self.v2_receipt()
        mutations=[lambda r:r['commands'][4].update(exitCode=1),lambda r:r['commands'][4].update(timedOut=True),
                   lambda r:r['commands'][4].update(waitReaped=False),lambda r:r['commands'][4].update(streamsClosed=False),
                   lambda r:r['commands'][4]['stdout'].update(eof=False),lambda r:r['commands'][4]['stdout'].update(base64=''),
                   lambda r:r['commands'][4]['stdout'].update(sha256='0'*64),lambda r:r['commands'][4]['directExit'].update(exitCode=1),
                   lambda r:r['commands'][4].update(elapsedSeconds=1200),lambda r:r['commands'][4].update(startedMonotonic=float('inf'))]
        for mutate in mutations:
            value=copy.deepcopy(original);mutate(value)
            with self.assertRaises(ValueError):C.validate_receipt(value,self.profile,C.pin(self.raw))
    def test_packed_proof_and_version_output_cannot_be_substituted(self):
        value=self.v2_receipt();value['proof']=copy.deepcopy(value['proof']);value['proof']['substituted']=True
        with self.assertRaises(ValueError):C.validate_receipt(value,self.profile,C.pin(self.raw))
        value=self.v2_receipt();body=b'v22.0.0\n';value['commands'][0]['stdout']={**C.pin(body),'base64':base64.b64encode(body).decode(),'eof':True}
        with self.assertRaises(ValueError):C.validate_receipt(value,self.profile,C.pin(self.raw))
    def test_dumpability_restoration_follows_verified_privilege_drop_in_source(self):
        source=(HERE/'namespace_init.py').read_text()
        self.assertLess(source.index('os.setuid(request'),source.index('libc.prctl(4,1'))
        self.assertLess(source.index('N.validate_snapshot(initial'),source.index('libc.prctl(4,1'))
        self.assertLess(source.index('libc.prctl(4,1'),source.index('subprocess.Popen'))
        value=self.v2_receipt();value['executionContext']['start']['initDumpable']=0
        with self.assertRaises(ValueError):C.validate_receipt(value,self.profile,C.pin(self.raw))
    def test_context_writer_meets_its_private_file_mode_contract(self):
        tree=ast.parse((HERE/'namespace_init.py').read_bytes())
        writer=next(n for n in tree.body if isinstance(n,ast.FunctionDef)and n.name=='write')
        source=ast.get_source_segment((HERE/'namespace_init.py').read_text(),writer)
        self.assertIn('os.fchmod(stream.fileno(),0o600)',source)
        self.assertLess(source.index('os.fchmod'),source.index('stream.write'))
    def test_nonroot_gid_is_fenced_before_namespace_and_context_effects(self):
        launcher=(HERE/'namespace_launch.py').read_text();init=(HERE/'namespace_init.py').read_text()
        self.assertIn("host['gid'] > 0",launcher);self.assertLess(launcher.index("host['gid'] > 0"),launcher.index('subprocess.run'))
        self.assertIn("request['gid'] == request['host']['gid'] > 0",init)
        self.assertLess(init.index("request['host']['gid'] > 0"),init.index('libc.prctl(38'))


if __name__=='__main__':unittest.main()
