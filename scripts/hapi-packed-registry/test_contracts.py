"""Finite modeled data controls only: never import producer.py or execute npm/cache libraries."""
import ast
import base64
import copy
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('registry_contracts',HERE/'contracts.py')
C = importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(C)
PROFILE_BYTES = (HERE/'profile.json').read_bytes(); NPM_BYTES = (HERE/'npm-members.json').read_bytes()
PROFILE, MEMBERS = C.load_contract(PROFILE_BYTES,NPM_BYTES)


def package_body(name, version, deps=None, root='package', extra=None):
    manifest = {'name':name,'version':version,**({'dependencies':deps}if deps else {})}
    entries = [(root+'/package.json',C.encoded(manifest),tarfile.REGTYPE,0o644)] + (extra or [])
    target = io.BytesIO()
    with tarfile.open(fileobj=target,mode='w:gz',format=tarfile.PAX_FORMAT)as archive:
        for path,body,kind,mode in entries:
            item=tarfile.TarInfo(path);item.type=kind;item.mode=mode;item.size=len(body)
            if kind in(tarfile.SYMTYPE,tarfile.LNKTYPE):item.linkname='../escape';item.size=0
            archive.addfile(item,io.BytesIO(body)if item.isfile()else None)
    return target.getvalue()


def fixture():
    specs={'mongodb':('7.2.0',{}),'ajv':('8.20.0',{}),'semver':('7.8.5',{}),
           '@types/node':('25.5.0',{'model-helper':'^1.0.0'}),'model-helper':('1.0.0',{})}
    fake='sha512-'+base64.b64encode(b'1'*64).decode()
    lock={'name':'stateful-native-consumer','lockfileVersion':3,'requires':True,'packages':{
        '':{'name':'stateful-native-consumer','dependencies':{C.LOCAL_NAMES[0]:'file:../a.tgz',C.LOCAL_NAMES[1]:'file:../b.tgz','@types/node':'25.5.0'}},
        'node_modules/'+C.LOCAL_NAMES[0]:{'version':'0.0.0-dev.1','resolved':'file:../a.tgz','integrity':fake,'dependencies':{'mongodb':'7.2.0'},'peerDependencies':{C.LOCAL_NAMES[1]:'0.0.0-dev.1'}},
        'node_modules/'+C.LOCAL_NAMES[1]:{'version':'0.0.0-dev.1','resolved':'file:../b.tgz','integrity':fake,'dependencies':{'ajv':'8.20.0','semver':'7.8.5'}}}}
    bodies={};catalog=[]
    def entry(key,body,metadata):
        integrity=C.body_sri(body);bodies[integrity]=body
        catalog.append({'key':key,'integrity':integrity,'size':len(body),'time':2,'originTime':1,'metadata':metadata})
    def meta(url,kind):
        return {'time':1,'url':url,'reqHeaders':{'accept':'application/vnd.npm.install-v1+json'},
                'resHeaders':{'content-type':kind},'options':{'compress':True}}
    for name,(version,deps)in specs.items():
        body=package_body(name,version,deps,root='node'if name=='@types/node'else'package')
        url=C.REGISTRY+name+'/-/'+name.split('/')[-1]+'-'+version+'.tgz';integrity=C.body_sri(body)
        row={'version':version,'resolved':url,'integrity':integrity,**({'dependencies':deps}if deps else{})}
        lock['packages']['node_modules/'+name]=row
        packument=C.encoded({'name':name,'versions':{version:{'name':name,'version':version,'dist':{'tarball':url,'integrity':integrity}}}})
        metadata_url=C.REGISTRY+name.replace('/','%2f')
        entry('make-fetch-happen:request-cache:'+metadata_url,packument,meta(metadata_url,'application/json'))
        entry('make-fetch-happen:request-cache:'+url,body,meta(url,'application/octet-stream'))
    return lock,catalog,bodies


def terminal():
    stdout=('\n'.join(['# tests 56','# pass 56','# fail 0','# cancelled 0','# skipped 0'])+'\n').encode()
    stream=lambda body:{**C.pin(body),'base64':base64.b64encode(body).decode(),'eof':True}
    return {'waitReaped':True,'exitCode':0,'timedOut':False,'elapsedSeconds':1,'streamsClosed':True,'descendantsClosed':True,
            'closureObservations':[{'at':1,'live':[]},{'at':2,'live':[]}],'readerErrors':[],'stdout':stream(stdout),'stderr':stream(b'')}


class Contracts(unittest.TestCase):
    def setUp(self):
        self.lock,self.entries,self.bodies=fixture()
        self.tmp=tempfile.TemporaryDirectory(prefix='tools94-pure-');self.root=Path(self.tmp.name).resolve();self.parent=self.root.parent
        self.addCleanup(self.cleanup)
    def cleanup(self):
        self.assertEqual(self.root.resolve().parent,self.parent);self.assertTrue(self.root.name.startswith('tools94-pure-'));self.assertFalse(self.root.is_symlink());self.tmp.cleanup()
    def validate(self):
        return C.validate_catalog(self.lock,self.entries,lambda integrity,size:self.bodies[integrity],PROFILE)

    def test_complete_modeled_metadata_and_transitive_tarball_closure(self):
        result=self.validate();self.assertEqual(len(result),10);self.assertEqual(sum('package'in r for r in result),5)
        self.assertTrue(any(r.get('package',{}).get('root')=='node'for r in result))
    def test_npm_saved_prefix_keeps_exact_selected_root_and_original_cli_cache_key(self):
        self.lock['packages']['']['dependencies']['@types/node']='^25.5.0'
        packages,requests=C.graph(self.lock,PROFILE)
        self.assertEqual(packages['node_modules/@types/node']['version'],'25.5.0')
        self.assertEqual(requests['@types/node'],{'25.5.0','^25.5.0'})
        # Pacote can retain the original explicit request, independently of npm's saved range.
        row=copy.deepcopy(self.entries[7]);row.update(key='pacote:tarball:@types/node@25.5.0',metadata=None)
        self.entries.append(row);self.validate()
    def test_other_saved_ranges_and_newer_root_versions_still_refuse(self):
        for spec in ('~25.5.0','*','^25.6.0','25.5.1'):
            lock=copy.deepcopy(self.lock);lock['packages']['']['dependencies']['@types/node']=spec
            with self.subTest(spec=spec):
                with self.assertRaises(ValueError):C.graph(lock,PROFILE)
        lock=copy.deepcopy(self.lock);lock['packages']['']['dependencies']['@types/node']='^25.5.0'
        lock['packages']['node_modules/model-helper/node_modules/@types/node']=copy.deepcopy(lock['packages']['node_modules/@types/node'])
        lock['packages']['node_modules/model-helper']['dependencies']={'@types/node':'25.5.0'}
        lock['packages']['node_modules/@types/node']['version']='25.5.1'
        with self.assertRaisesRegex(ValueError,'exact root @types/node'):C.graph(lock,PROFILE)
    def test_fixed_raw_runtime_inventory_not_a_version_label(self):
        C.verify_runtime_members(MEMBERS['entries'],copy.deepcopy(MEMBERS['entries']))
        changed=copy.deepcopy(MEMBERS['entries']);changed[-1]['mode']^=1
        with self.assertRaises(ValueError):C.verify_runtime_members(changed,MEMBERS['entries'])
        with self.assertRaises(ValueError):C.verify_runtime_members(changed[:-1],MEMBERS['entries'])
    def test_raw_runtime_body_no_newline_normalization(self):
        with self.assertRaises(ValueError):C.load_contract(PROFILE_BYTES,NPM_BYTES.replace(b'\n',b'\r\n'))
    def test_profile_source_sdk_runtime_and_budget_drift(self):
        mutations=[lambda p:p['product'].update(productHead='0'*40),lambda p:p['runtime'].update(npm='11.19.1'),
          lambda p:p['product']['dependencySources']['rootSdk'].update(resolved='git+https://github.com/eosrio/hyperion-sdk.git#429a1bf1d3bddb746fa662de9529480890b7368f'),
          lambda p:p['product']['dependencySources']['adapterSdk'].update(resolved='git+https://github.com/eosrio/hyperion-sdk.git#429a1bf1d3bddb746fa662de9529480890b7368f'),
          lambda p:p['runtime']['npmPackage'].update(sha256='0'*64),lambda p:p['retainedReceiverLimits'].update(componentSeconds=301),
          lambda p:p['offlineEnvironment'].update(npm_config_offline='prefer-offline'),lambda p:p['artifactPins'].update(zip={'sha256':'a'*64}),
          lambda p:p.update(workflow='.github/workflows/other.yml'),lambda p:p['artifact'].update(outputs='seed.tar.gz'),
          lambda p:p.update(executionReady=False)]
        for mutate in mutations:
            p=copy.deepcopy(PROFILE);mutate(p)
            with self.subTest(profile=p):
                with self.assertRaises(ValueError):C.validate_profile(p,MEMBERS)
    def test_unreferenced_or_local_payloads_cannot_enter_registry_seed(self):
        for key in ['pacote:tarball:file:/tmp/local.tgz','pacote:tarball:git+https://github.com/x/y','make-fetch-happen:request-cache:https://registry.npmjs.org/unreferenced']:
            row=copy.deepcopy(self.entries[1]);row['key']=key;row['metadata']=None
            self.entries.append(row)
            with self.subTest(key=key):
                with self.assertRaises(ValueError):self.validate()
            self.entries.pop()
    def test_missing_metadata_or_tarball_never_forms_closure(self):
        for index in(0,1):
            removed=self.entries.pop(index)
            with self.assertRaises(ValueError):self.validate()
            self.entries.insert(index,removed)
    def test_cache_bytes_must_match_both_sri_and_size(self):
        integrity=self.entries[0]['integrity'];self.bodies[integrity]+=b' '
        with self.assertRaises(ValueError):self.validate()
    def test_metadata_selected_tarball_integrity_is_independently_checked(self):
        row=self.entries[0];body=C.parse_json(self.bodies[row['integrity']]);body['versions']['7.2.0']['dist']['integrity']=C.body_sri(b'wrong')
        changed=C.encoded(body);row.update(integrity=C.body_sri(changed),size=len(changed));self.bodies[row['integrity']]=changed
        with self.assertRaises(ValueError):self.validate()
    def test_auth_cookie_and_extra_http_options_refuse(self):
        for field,key,value in [('reqHeaders','authorization','Bearer secret'),('resHeaders','set-cookie','secret'),('options','proxy','http://proxy')]:
            row=self.entries[0];row['metadata'][field][key]=value
            with self.subTest(field=field):
                with self.assertRaises(ValueError):self.validate()
            del row['metadata'][field][key]
    def test_registry_url_credentials_query_fragment_and_escape_refuse(self):
        for url in ['http://registry.npmjs.org/x','https://token@registry.npmjs.org/x','https://registry.npmjs.org/x?token=x',
                    'https://registry.npmjs.org/x#x','https://evil.invalid/x','https://registry.npmjs.org/%2e%2e/x']:
            with self.subTest(url=url):
                with self.assertRaises(ValueError):C.registry_url(url)
    def test_lock_graph_requires_transitives_and_no_extra_nodes(self):
        del self.lock['packages']['node_modules/model-helper']
        with self.assertRaises(ValueError):self.validate()
        self.lock,self.entries,self.bodies=fixture();self.lock['packages']['node_modules/unused']=copy.deepcopy(self.lock['packages']['node_modules/semver'])
        with self.assertRaises(ValueError):self.validate()
    def test_registry_tar_dependency_contents_bind_lock(self):
        body=package_body('@types/node','25.5.0',{'different':'1.0.0'},root='node');new=C.body_sri(body)
        selected=self.lock['packages']['node_modules/@types/node'];old=selected['integrity'];selected['integrity']=new;self.bodies[new]=body
        for row in self.entries:
            if row['integrity']==old:row.update(integrity=new,size=len(body))
        meta=self.entries[6];value=C.parse_json(self.bodies[meta['integrity']]);value['versions']['25.5.0']['dist']['integrity']=new
        data=C.encoded(value);meta.update(integrity=C.body_sri(data),size=len(data));self.bodies[meta['integrity']]=data
        with self.assertRaises(ValueError):self.validate()
    def test_package_tar_rejects_escape_duplicate_link_device_and_mode(self):
        bad=[('package/../escape',b'x',tarfile.REGTYPE,0o644),('package/package.json',b'{}',tarfile.REGTYPE,0o644),
             ('package/link',b'',tarfile.SYMTYPE,0o755),('package/link',b'',tarfile.LNKTYPE,0o755),('package/dev',b'',tarfile.CHRTYPE,0o644),
             ('package/writable',b'x',tarfile.REGTYPE,0o666)]
        for extra in bad:
            with self.subTest(extra=extra):
                with self.assertRaises(ValueError):C.package_tar(package_body('modeled','1.0.0',extra=[extra]))
    def test_hidden_second_gzip_payload_refuses(self):
        body=package_body('modeled','1.0.0')+gzip.compress(b'hidden')
        with self.assertRaises(ValueError):C.package_tar(body)
    def test_duplicate_json_sri_algorithms_and_bool_sizes_refuse(self):
        with self.assertRaises(ValueError):C.parse_json(b'{"x":1,"x":2}')
        for value in ['sha1-aaa',self.entries[0]['integrity']+' '+self.entries[0]['integrity'],'sha512-bad']:
            with self.assertRaises(ValueError):C.sri(value)
        self.entries[0]['size']=True
        with self.assertRaises(ValueError):self.validate()
    def test_cache_actual_index_content_refs_and_unreferenced_members(self):
        rows=[]
        for row in self.entries:
            for name,body in [(C.content_path(row['integrity']),self.bodies[row['integrity']]),
                (C.index_path(row['key']),b'')]:
                path=self.root/name;path.parent.mkdir(parents=True,exist_ok=True)
                if body:path.write_bytes(body)
            actual={k:row[k]for k in('key','integrity','time','size','metadata')};text=json.dumps(actual,separators=(',',':')).encode()
            (self.root/C.index_path(row['key'])).write_bytes(b'\n'+hashlib.sha1(text).hexdigest().encode()+b'\t'+text)
        for path in self.root.rglob('*'):
            rel=path.relative_to(self.root).as_posix();rows.append({'path':rel,'type':'directory','mode':0o755}if path.is_dir()else{'path':rel,'type':'file','mode':0o644,**C.pin(path.read_bytes())})
        # NTFS modes are not POSIX evidence; this modeled scanner seam exercises actual index/body bytes.
        with mock.patch.object(C,'cache_inventory',return_value=rows):C.validate_cache_tree(self.root,self.entries,self.lock,PROFILE)
        extra=self.root/'_cacache/unreferenced';extra.write_bytes(b'private')
        with mock.patch.object(C,'cache_inventory',return_value=rows+[{'path':'_cacache/unreferenced','type':'file','mode':0o644,**C.pin(b'private')}]):
            with self.assertRaises(ValueError):C.validate_cache_tree(self.root,self.entries,self.lock,PROFILE)
    def test_cache_bad_index_checksum_and_duplicate_records_refuse(self):
        row=self.entries[0];path=self.root/C.index_path(row['key']);path.parent.mkdir(parents=True);path.write_bytes(b'\nwrong\t{}')
        with mock.patch.object(C,'cache_inventory',return_value=[]):
            with self.assertRaises(ValueError):C.validate_cache_tree(self.root,self.entries[:1],self.lock,PROFILE)
    def test_archive_file_modes_bytes_member_set_and_tail(self):
        body=b'cache';rows=[{'path':'_cacache','type':'directory','mode':0o755},{'path':'_cacache/x','type':'file','mode':0o644,**C.pin(body)}]
        path=self.root/'seed.tar.gz'
        with tarfile.open(path,'w:gz')as archive:
            item=tarfile.TarInfo('_cacache');item.type=tarfile.DIRTYPE;item.mode=0o755;archive.addfile(item)
            item=tarfile.TarInfo('_cacache/x');item.mode=0o644;item.size=len(body);archive.addfile(item,io.BytesIO(body))
        C.verify_archive(path,rows)
        bad=copy.deepcopy(rows);bad[1]['sha256']='0'*64
        with self.assertRaises(ValueError):C.verify_archive(path,bad)
        with path.open('ab')as stream:stream.write(gzip.compress(b'hidden'))
        with self.assertRaises(ValueError):C.verify_archive(path,rows)
    def test_receiving_target_freshness_without_extracting_anything(self):
        target=self.root/'cache';self.assertTrue(C.fresh_target(self.root,target));target.mkdir()
        with self.assertRaises(ValueError):C.fresh_target(self.root,target)
        with self.assertRaises(ValueError):C.fresh_target(self.root,self.root/'missing'/'nested')
    def test_original_failed_wait_timeout_or_stream_never_becomes_pass(self):
        C.validate_terminal(terminal())
        for key,value in [('exitCode',1),('waitReaped',False),('timedOut',True),('streamsClosed',False),('descendantsClosed',False),('elapsedSeconds',60),('closureObservations',[])]:
            row=terminal();row[key]=value
            with self.subTest(key=key):
                with self.assertRaises(ValueError):C.validate_terminal(row)
        row=terminal();row['stdout']['eof']=False
        with self.assertRaises(ValueError):C.validate_terminal(row)
    def test_uncensored56_control_stream_and_data_only_receipt(self):
        proof=terminal();row={'schemaVersion':1,'kind':PROFILE['kind'],'profile':C.pin(PROFILE_BYTES),'product':PROFILE['product'],'consumer':PROFILE['consumer'],
            'producer':{'repository':'eosrio/hyperion-tools','workflowPath':PROFILE['workflow'],'job':PROFILE['job'],'workflowRevision':'a'*40,'workflowTree':'b'*40,'runId':1,'runAttempt':1},
            'runtime':PROFILE['runtime'],'offlineEnvironment':PROFILE['offlineEnvironment'],'scope':{'isolatedPackedConsumer':True,'fullWireComponent':'NOT_RUN','receiverLimitsUnchanged':PROFILE['retainedReceiverLimits']},
            'seedSealedBeforeProof':True,'seedContainsLocalPacksOrBuilds':False,'freshProofSourceAndBuild':True,'proof':proof,
            'actualOutputPacks':[{'name':name,'role':'OUTPUT','origin':'actual-proof-mutable-cache-after-original-terminal'}for name in C.LOCAL_NAMES],'postUploadArtifact':None}
        C.validate_receipt(row,PROFILE,C.pin(PROFILE_BYTES))  # Schema control only; all profile artifact pins still null.
        self.assertTrue(all(value is None for value in PROFILE['artifactPins'].values()))
        for mutate in [lambda r:r['scope'].update(fullWireComponent='PASS'),lambda r:r.update(executionReady=True),lambda r:r.update(actualOutputPacks=[]),lambda r:r['proof'].update(exitCode=1)]:
            changed=copy.deepcopy(row);mutate(changed)
            with self.assertRaises(ValueError):C.validate_receipt(changed,PROFILE,C.pin(PROFILE_BYTES))
    def test_producer_and_bridge_compile_without_import_or_execution(self):
        for name in('producer.py','contracts.py'):
            tree=ast.parse((HERE/name).read_bytes(),filename=name)
            self.assertFalse(any(isinstance(n,ast.Attribute)and n.attr in('kill','terminate','send_signal')for n in ast.walk(tree)))
        self.assertNotIn('producer',C.__dict__)

    def test_old_current103_inputs_stay_byte_for_byte_and_new_workflow_is_explicit(self):
        tools=HERE.parents[1]
        self.assertEqual(C.pin((tools/'scripts/hapi-linux-dependencies/producer.py').read_bytes()),
                         {'bytes':32453,'sha256':'bf36ec5137cac4703e4af18f22585cbc9bf39a86e80375585cca828a2b41b8a3'})
        self.assertEqual(C.pin((tools/'scripts/hapi-linux-dependencies/profile-current103.json').read_bytes()),
                         {'bytes':2469,'sha256':'79629c06879a8ad4795ec725a9297d2004a5f4dbde52501b575fe6ba4bbcf28b'})
        workflow=(tools/PROFILE['workflow']).read_text()
        for required in ['cancel-in-progress: false','timeout-minutes: 45',"node-version: '26.4.0'",'registry-supplement:',
                         'ref: bca518ddfebf04c2af957832f2ad9c0582c17932','scripts/hapi-packed-registry/producer.py',
                         'current103-packed-registry-v1.OUTPUT.tar.gz','if: failure()']:
            self.assertIn(required,workflow)
        self.assertNotIn('continue-on-error',workflow)


if __name__=='__main__':unittest.main()
