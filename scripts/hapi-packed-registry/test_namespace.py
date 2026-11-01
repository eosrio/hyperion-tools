"""Pure namespace/process models and AST controls only; no execution-entry import."""
import ast
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
            'pidNamespace':'pid:[20]','mountNamespace':'mnt:[21]','procSelf':str(pid),'initNamespace':'pid:[20]',
            'init':{'pid':1,'birth':100},'procMounted':True,'capEff':'0000000000000000','noNewPrivs':'1'}
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
    def test_v2_producer_receipt_requires_original_context_and_every_command_proof(self):
        model=importlib.import_module('test_contracts');proof=model.terminal();pin=C.pin(self.raw);start_pin=C.pin(b'modeled-context')
        start={'kind':N.KIND,'profile':pin,'host':host(),'uid':1001,'gid':1001,'init':snapshot(1)}
        stage={'contextPin':start_pin,'actual':snapshot()}
        proof.update(closurePolicy=N.KIND,diagnosticErrors=[],namespaceBefore=stage,namespaceAfter=stage,
                     lastCompleteCensus={'complete':True,'live':[]},closureObservations=[{'complete':True,'live':[]},{'complete':True,'live':[]}])
        receipt={'schemaVersion':2,'kind':self.profile['kind'],'profile':pin,'product':self.profile['product'],'consumer':self.profile['consumer'],
                 'producer':{'repository':'eosrio/hyperion-tools','workflowPath':self.profile['workflow'],'job':self.profile['job'],
                             'workflowRevision':'a'*40,'workflowTree':'b'*40,'runId':1,'runAttempt':1},
                 'runtime':self.profile['runtime'],'offlineEnvironment':self.profile['offlineEnvironment'],
                 'scope':{'isolatedPackedConsumer':True,'fullWireComponent':'NOT_RUN','receiverLimitsUnchanged':self.profile['retainedReceiverLimits']},
                 'seedSealedBeforeProof':True,'seedContainsLocalPacksOrBuilds':False,'freshProofSourceAndBuild':True,'proof':proof,
                 'actualOutputPacks':[{'name':name,'role':'OUTPUT','origin':'actual-proof-mutable-cache-after-original-terminal'}for name in C.LOCAL_NAMES],
                 'postUploadArtifact':None,'commands':[proof],
                 'executionContext':{'kind':N.KIND,'contextPin':start_pin,'start':start,'before':stage,'after':stage}}
        C.validate_receipt(receipt,self.profile,pin)
        for mutate in [lambda r:r['commands'][0]['lastCompleteCensus'].update(complete=False),
                       lambda r:r['commands'][0].update(diagnosticErrors=['capture failed']),
                       lambda r:r['executionContext']['start'].update(profile=C.pin(b'wrong'))]:
            bad=copy.deepcopy(receipt);mutate(bad)
            with self.assertRaises(ValueError):C.validate_receipt(bad,self.profile,pin)
        old=C.parse_json((HERE/'profile.json').read_bytes())
        with self.assertRaises(ValueError):C.validate_receipt(receipt,old,C.pin((HERE/'profile.json').read_bytes()))


if __name__=='__main__':unittest.main()
