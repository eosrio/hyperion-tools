// Source-only controls: no product import, installs, workload, server or child process.
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {assessCoverage, inspectProfile, workloadVerdict} from './evidence.mjs';
import {cases, selectCase} from './cases.mjs';

const prefixes = ['file:///product/build/', 'file:///product/node_modules/@eosrio/hyperion-sdk/'];
const target = '/product/tests/unit/wire-managed-state-v2-native.mjs';
const worker = {active: true, pid: 102, ppid: 101, context: 'child-v8', argv: ['/node', target]};
const passedTap = '# tests 2\n# pass 2\n# fail 0\n# cancelled 0\n# skipped 0\n# todo 0\n';
const profile = () => ({startTime: 100, endTime: 4100, nodes: [
    {id: 1, callFrame: {functionName: '(root)', url: ''}, children: [2]},
    {id: 2, callFrame: {functionName: 'actualHost', url: prefixes[0] + 'host.js'}, children: [3]},
    {id: 3, callFrame: {functionName: 'actualSdk', url: prefixes[1] + 'build/index.js'}}],
    samples: [3, 3, 2], timeDeltas: [1000, 1000, 1000]});
const fixture = () => ({identities: [{...worker, active: false, pid: 101}, worker],
    exits: [{identity: worker, sequence: 1, profiling: false, stopping: false, errors: [], naturalExitObserved: true, elapsedMs: 4}],
    states: [{identity: worker, chunkStarted: 0, errors: []}], profiles: [{file: 'process-102.chunk-00.cpuprofile',
        receipt: {identity: worker, sequence: 0, completed: true, reason: 'beforeExit', chunkStartedMs: 0, chunkEndedMs: 4}, profile: profile()}],
    requiredPrefixes: prefixes, runnerPid: 101, target});
test('actual worker identity and sampled HAPI/SDK ancestry are both required', () => {
    const result = assessCoverage(fixture()); assert.equal(result.verdict, 'PASS');
    assert.equal(result.completedChunks[0].analysis.samples, 3);
    assert.equal(result.completedChunks[0].analysis.matched[0].urls.length, 1);
});
test('parent-only profile cannot satisfy worker coverage', () => {
    const input = fixture(); input.identities = [input.identities[0]];
    assert.equal(assessCoverage(input).verdict, 'FAIL');
});
test('worker process context, parent and exact target must agree', () => {
    for (const patch of [{context: null}, {ppid: 9}, {pid: 101}, {argv: ['/node', '/another-test.mjs']}]) {
        const input = fixture(); input.identities[1] = {...worker, ...patch};
        assert.equal(assessCoverage(input).verdict, 'FAIL');
    }
});
test('a loaded module name without a sampled stack does not establish coverage', () => {
    const input = fixture(); input.profiles[0].profile.samples = [1, 1, 1];
    assert.equal(assessCoverage(input).verdict, 'FAIL');
});
test('missing SDK or HAPI sampled path is an explicit failure', () => {
    for (const prefix of prefixes) {
        const input = fixture(); for (const node of input.profiles[0].profile.nodes) if (node.callFrame.url.startsWith(prefix)) node.callFrame.url = '';
        assert.equal(assessCoverage(input).verdict, 'FAIL');
    }
});
test('absolute Linux script URLs are canonicalized while preserving raw frame evidence', () => {
    const input = fixture();
    for (const node of input.profiles[0].profile.nodes) node.callFrame.url = node.callFrame.url.replace('file://', '');
    assert.equal(assessCoverage(input).verdict, 'PASS');
});
test('missing and malformed or partial profiles fail', () => {
    const missing = fixture(); missing.profiles = []; assert.equal(assessCoverage(missing).verdict, 'FAIL');
    for (const patch of [{samples: []}, {timeDeltas: [1]}, {timeDeltas: [1, -2, 3]}, {endTime: 99}, {samples: [9, 9, 9]}]) {
        assert.throws(() => inspectProfile({...profile(), ...patch}, prefixes));
    }
});
test('completed periodic profiles remain inspectable when timeout censors final interval', () => {
    const input = fixture(); input.exits = []; input.states[0].chunkStarted = 150000;
    const result = assessCoverage(input); assert.equal(result.verdict, 'FAIL');
    assert.equal(result.completedChunks.length, 1); assert.equal(result.censoredInterval.fromWorkerElapsedMs, 150000);
    assert.equal(result.censoredInterval.upperBoundKnown, false);
});
test('active, failed, bounded-out or missing flush receipts cannot pass full lifetime coverage', () => {
    for (const patch of [{profiling: true}, {stopping: true}, {boundedLimitReached: true}, {errors: ['write failed']}, {sequence: 2}]) {
        const input = fixture(); input.exits[0] = {...input.exits[0], ...patch};
        assert.equal(assessCoverage(input).verdict, 'FAIL');
    }
});
test('duplicate or skipped chunk sequence and parse errors fail coverage', () => {
    for (const sequence of [undefined, 1, 10]) {
        const input = fixture(); input.profiles[0].receipt.sequence = sequence;
        assert.equal(assessCoverage(input).verdict, 'FAIL');
    }
    const parsed = fixture(); parsed.profiles[0].parseError = 'truncated JSON';
    assert.equal(assessCoverage(parsed).verdict, 'FAIL');
});
test('collection of a timeout is never a test pass', () => {
    const wait = {code: 1, signal: null, closeObserved: true, stdoutEnded: true, stderrEnded: true};
    const result = workloadVerdict(wait, 'test timed out after 180000ms');
    assert.equal(result.verdict, 'FAIL'); assert.equal(result.timedOut, true); assert.equal(result.originalDeadlineMs, 180000);
});
test('natural exit and both stream ends are mandatory even when code is zero', () => {
    const wait = {code: 0, signal: null, closeObserved: true, stdoutEnded: true, stderrEnded: true};
    assert.equal(workloadVerdict(wait, passedTap).verdict, 'PASS');
    for (const patch of [{signal: 'SIGTERM'}, {closeObserved: false}, {stdoutEnded: false}, {stderrEnded: false}, {truncated: true}, {stderrError: 'broken'}])
        assert.equal(workloadVerdict({...wait, ...patch}, passedTap).verdict, 'FAIL');
});
test('zero exit without both original assertions or with skips cannot pass', () => {
    const wait = {code: 0, signal: null, closeObserved: true, stdoutEnded: true, stderrEnded: true};
    for (const text of ['', passedTap.replace('# pass 2', '# pass 1'), passedTap.replace('# skipped 0', '# skipped 1'), passedTap + '# pass 2\n', passedTap + 'test timed out after 180000ms'])
        assert.equal(workloadVerdict(wait, text).verdict, 'FAIL');
});
test('CPU trees with duplicate, dangling or cyclic nodes are refused', () => {
    const duplicate = profile(); duplicate.nodes.push(duplicate.nodes[0]); assert.throws(() => inspectProfile(duplicate, prefixes));
    const dangling = profile(); dangling.nodes[0].children.push(90); assert.throws(() => inspectProfile(dangling, prefixes));
    const cyclic = profile(); cyclic.nodes[2].children = [1]; assert.throws(() => inspectProfile(cyclic, prefixes));
});
test('workflow pins original runtime/source and retains failure artifacts', () => {
    const workflow = readFileSync(new URL('../../.github/workflows/wire-managed-linux-profile.yml', import.meta.url), 'utf8');
    assert(workflow.includes('24.22.1')); assert(workflow.includes('ubuntu-24.04'));
    assert(workflow.includes('21ef67d84ad94fb35a0c1bba600a42df5318c404'));
    assert(workflow.includes('if: always()')); assert(workflow.includes('persist-credentials: false'));
    assert(!workflow.includes('continue-on-error')); assert(!workflow.includes('pull_request_target'));
});


test('only the two fixed failed-environment cases are selectable', () => {
    assert.deepEqual(Object.keys(cases), ['node24-21ef', 'node26-404d']);
    assert.deepEqual(selectCase('node24-21ef'), {source: '21ef67d84ad94fb35a0c1bba600a42df5318c404',
        tree: '95a331dbe4ba2b9438db7afabef35f57c35fe032', node: 'v24.22.1', npm: '11.19.1',
        issue: 'https://github.com/eosrio/hyperion-tools/issues/83', failedRun: '41256269394', failedJob: '124699981577', report: 'report.json'});
    assert.deepEqual(selectCase('node26-404d'), {source: '404d36f58aaaf83b5b25c8e5a62d38ed582f5d57',
        tree: 'df4e4527bc6a3a34c5f046b5f6f5f192c382b66e', node: 'v26.13.0', npm: '11.20.0',
        issue: 'https://github.com/eosrio/hyperion-tools/issues/86', failedRun: '41262118567', failedJob: '124717329561', report: 'report-node26-404d.json'});
    for (const id of ['__proto__', 'constructor', '', 'node26', undefined]) assert.throws(() => selectCase(id), /Unknown fixed/);
    assert(Object.isFrozen(cases) && Object.values(cases).every(Object.isFrozen));
});

test('Node 26 workflow selects the exact new case and retains failures', () => {
    const workflow = readFileSync(new URL('../../.github/workflows/wire-managed-linux-node26-profile.yml', import.meta.url), 'utf8');
    assert(workflow.includes('404d36f58aaaf83b5b25c8e5a62d38ed582f5d57'));
    assert(workflow.includes('node-version: 26.13.0')); assert(workflow.includes('run.mjs node26-404d '));
    assert(workflow.includes('wire-managed-linux-profile-node26-404d-'));
    assert(workflow.includes('if: always()') && !workflow.includes('continue-on-error'));
});
