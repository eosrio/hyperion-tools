import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {closeSync, existsSync, lstatSync, mkdirSync, openSync, readFileSync, readdirSync, realpathSync, writeFileSync, writeSync} from 'node:fs';
import {join, dirname, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import os from 'node:os';
import {performance} from 'node:perf_hooks';
import {assessCoverage, workloadVerdict} from './evidence.mjs';
import {selectCase} from './cases.mjs';

assert(process.argv.length === 5, 'Usage: node run.mjs CASE_ID PRODUCT_ROOT NEW_OUTPUT_DIRECTORY');
const caseId = process.argv[2], selected = selectCase(caseId);
const SOURCE = selected.source, TREE = selected.tree;
const PROFILE = 'stateful-owner-v2-dd85ee5-18d235e-fields-engineering';
const here = dirname(fileURLToPath(import.meta.url));
const product = resolve(process.argv[3]), out = resolve(process.argv[4]);
assert.equal(process.platform, 'linux', 'Real workload is Linux CI only');
assert.equal(process.version, selected.node, 'Reproduce exact failed-job Node version');
assert.equal(process.env.CI, 'true', 'Real workload is explicitly CI scoped');
assert(!process.env.NODE_OPTIONS && !process.env.NODE_TEST_CONTEXT, 'Refuse inherited Node instrumentation or nested runner');
assert(!existsSync(out), 'Output must be a new owned directory');
assert(!out.startsWith(product + '/') && !product.startsWith(out + '/'), 'Output and product source must be separate');
mkdirSync(out, {recursive: true});
const json = (name, value) => writeFileSync(join(out, name), JSON.stringify(value, null, 2) + '\n');
const hash = path => createHash('sha256').update(readFileSync(path)).digest('hex');
const readOptional = path => {try {return readFileSync(path, 'utf8');} catch (error) {return {unavailable: error.code};}};
const report = {schema: 'hyperion-wire-managed-linux-profile/v1', caseId, source: SOURCE, sourceTree: TREE,
    issue: selected.issue, originalFailedRun: selected.failedRun, originalFailedJob: selected.failedJob,
    originalDeadlineMs: 180000, sourceProfile: PROFILE, startedAt: new Date().toISOString(),
    workload: {verdict: 'NOT_RUN'}, coverage: {verdict: 'NOT_RUN'}, collection: {verdict: 'INCOMPLETE'}, phases: []};
const save = () => json(selected.report, report);
save();
const baseEnv = {...process.env, HYPERION_NATIVE_SOURCE_PROFILE: PROFILE};
async function command(name, argv, {cwd = product, env = baseEnv} = {}) {
    const start = performance.now(), fds = {}, count = {stdout: 0, stderr: 0};
    const result = {name, argv, cwd, startedAt: new Date().toISOString(), code: null, signal: null,
        closeObserved: false, stdoutEnded: false, stderrEnded: false, spawnError: null, truncated: false};
    report.phases.push(result); save();
    for (const stream of ['stdout', 'stderr']) fds[stream] = openSync(join(out, `${name}.${stream}.log`), 'wx');
    const child = spawn(argv[0], argv.slice(1), {cwd, env, stdio: ['ignore', 'pipe', 'pipe']});
    result.pid = child.pid;
    for (const stream of ['stdout', 'stderr']) {
        child[stream].on('data', data => {
            const budget = 64 * 1024 * 1024;
            const retained = data.subarray(0, Math.max(0, budget - count[stream]));
            try {if (retained.length) writeSync(fds[stream], retained);} catch (error) {result[`${stream}Error`] = String(error);}
            if (retained.length !== data.length) result.truncated = true;
            count[stream] += data.length;
        });
        child[stream].once('end', () => {result[`${stream}Ended`] = true;});
        child[stream].once('error', error => {result[`${stream}Error`] = String(error);});
    }
    child.once('error', error => {result.spawnError = String(error);});
    child.once('exit', (code, signal) => {result.code = code; result.signal = signal; result.exitObservedAt = new Date().toISOString(); save();});
    // No kill, timeout, ancestry lookup or automatic retry. CI's outer job deadline remains a failure boundary.
    await new Promise(resolveWait => child.once('close', (code, signal) => {
        result.code = code; result.signal = signal; result.closeObserved = true; resolveWait();
    }));
    for (const fd of Object.values(fds)) closeSync(fd);
    Object.assign(result, {elapsedMs: performance.now() - start, closedAt: new Date().toISOString(), bytes: count,
        stdoutSha256: hash(join(out, `${name}.stdout.log`)), stderrSha256: hash(join(out, `${name}.stderr.log`))});
    save();
    return result;
}
const successful = row => row.code === 0 && row.signal === null && row.closeObserved && row.stdoutEnded && row.stderrEnded && !row.spawnError && !row.truncated && !row.stdoutError && !row.stderrError;
const textOf = name => readFileSync(join(out, `${name}.stdout.log`), 'utf8').trim();
async function requireCommand(name, argv, options) {const result = await command(name, argv, options); assert(successful(result), `${name} failed; raw result retained`); return result;}
function inventory(directory) {
    const files = [];
    function walk(path) {
        assert(files.length < 20000, 'bounded emission inventory');
        for (const entry of readdirSync(path, {withFileTypes: true})) {
            const file = join(path, entry.name); assert(!lstatSync(file).isSymbolicLink(), 'refuse emission symlink');
            if (entry.isDirectory()) walk(file); else if (entry.isFile()) files.push({file, sha256: hash(file)});
        }
    }
    walk(directory); return files.sort((a,b) => a.file.localeCompare(b.file));
}
function captureCoverage(directory, wait, target, prefixes) {
    const files = readdirSync(directory).sort();
    const rows = suffix => files.filter(file => file.endsWith(suffix)).map(file => JSON.parse(readFileSync(join(directory, file))));
    const profiles = files.filter(file => file.endsWith('.cpuprofile')).map(file => {
        try {return {file, sha256: hash(join(directory, file)), profile: JSON.parse(readFileSync(join(directory, file))),
            receipt: JSON.parse(readFileSync(join(directory, file + '.receipt.json')))};}
        catch (error) {return {file, receipt: {}, profile: {}, parseError: String(error)};}
    });
    return assessCoverage({identities: rows('.identity.json'), exits: rows('.exit.json'), states: rows('.state.json'),
        profiles, requiredPrefixes: prefixes, runnerPid: wait.pid, target});
}

try {
    json('host.json', {platform: os.platform(), release: os.release(), arch: os.arch(), cpus: os.cpus(),
        availableParallelism: os.availableParallelism(), totalmem: os.totalmem(), freemem: os.freemem(), loadavg: os.loadavg(),
        node: {execPath: process.execPath, executableSha256: hash(process.execPath), versions: process.versions, config: process.config},
        osRelease: readOptional('/etc/os-release'), cgroup: readOptional('/proc/self/cgroup'),
        cpuinfo: readOptional('/proc/cpuinfo'), meminfo: readOptional('/proc/meminfo'),
        cgroupMountinfo: readOptional('/proc/self/mountinfo'),
        cgroupView: 'Raw files at mounted /sys/fs/cgroup root; resolve against retained membership/mountinfo before attributing limits.',
        cgroupFiles: Object.fromEntries(['cpu.max','cpu.stat','cpuset.cpus.effective','memory.max','memory.current','memory.events','io.stat']
            .map(file => [file, readOptional('/sys/fs/cgroup/' + file)])),
        github: Object.fromEntries(['GITHUB_RUN_ID','GITHUB_RUN_ATTEMPT','GITHUB_SHA','GITHUB_WORKFLOW','RUNNER_OS','ImageOS','ImageVersion'].map(key => [key, process.env[key] ?? null]))});
    // Retain the exact installed runner implementation as well as documented semantics.
    const runnerSource = process.binding('natives')['internal/test_runner/runner'];
    assert(runnerSource?.includes('NODE_TEST_CONTEXT') && runnerSource.includes('filterExecArgv'), 'missing actual runner source');
    writeFileSync(join(out, 'node-test-runner-source.js'), runnerSource);
    await requireCommand('node-help', [process.execPath, '--help']);
    await requireCommand('source-head', ['git', 'rev-parse', 'HEAD']); assert.equal(textOf('source-head'), SOURCE);
    await requireCommand('source-tree', ['git', 'rev-parse', 'HEAD^{tree}']); assert.equal(textOf('source-tree'), TREE);
    await requireCommand('source-clean-before', ['git', 'diff', '--exit-code', 'HEAD', '--']);
    const testPath = join(product, 'tests/unit/wire-managed-state-v2-native.mjs');
    const testBytes = readFileSync(testPath, 'utf8');
    assert(testBytes.includes('{timeout: 180000}') && testBytes.includes('interleaved retained Wire 1..1480'));
    report.inputs = Object.fromEntries(['package.json', 'package-lock.json', 'packages/backend-mongodb/package-lock.json', '.bun-version',
        'tests/unit/wire-managed-state-v2-native.mjs','tests/fixtures/portable-state-worker/manifest.json']
        .map(file => [file, {sha256: hash(join(product, file))}])); save();
    const npmCli = realpathSync(join(dirname(process.execPath), 'npm'));
    report.npmCli = {path: npmCli, sha256: hash(npmCli)};
    await requireCommand('npm-version', [process.execPath, npmCli, '--version']); assert.equal(textOf('npm-version'), selected.npm);
    // Reproduce the failed job's install sequence. No lock rewriting or separate adapter install.
    await requireCommand('npm-ci', [process.execPath, npmCli, 'ci', '--allow-git=all', '--ignore-scripts=false']);
    await requireCommand('bun-version', ['bun', '--version']); assert.equal(textOf('bun-version'), '1.4.4');
    await requireCommand('bun-typescript', ['bun', '-p', 'process.versions.typescript']); assert.equal(textOf('bun-typescript'), '7.0.2');
    const ts = JSON.parse(readFileSync(join(product, 'node_modules/typescript/package.json')));
    assert.equal(ts.version, '7.0.2');
    await requireCommand('typescript-version', [process.execPath, join(product, 'node_modules/typescript/bin/tsc'), '--version']);
    await requireCommand('typecheck', [process.execPath, npmCli, 'run', 'typecheck']);
    await requireCommand('build', [process.execPath, npmCli, 'run', 'build']);
    json('installed-packages.json', Object.fromEntries(['@eosrio/hyperion-sdk','@eosrio/hyperion-wire-plugin','@eosrio/node-abieos','typescript']
        .map(name => [name, {package: JSON.parse(readFileSync(join(product, 'node_modules', name, 'package.json'))),
            packageSha256: hash(join(product, 'node_modules', name, 'package.json'))}])));
    json('fresh-emissions.json', {core: inventory(join(product, 'build')), adapter: inventory(join(product, 'packages/backend-mongodb/dist'))});
    const controls = ['tests/native/pinned-git-source.test.mjs', 'tests/unit/portable-state-owner-v2.mjs',
        'tests/unit/stateful-host-native.mjs', 'tests/unit/stateful-metadata-fields-source-admission-native.mjs'];
    for (const [index, file] of controls.entries()) await requireCommand(`prerequisite-${index}`, [process.execPath, ...(index === 0 ? ['--test'] : []), file]);
    // Verify --import reaches an actual isolated test process before spending the 180s workload.
    const canaryPath = join(out, 'wire-profile-canary.mjs');
    writeFileSync(canaryPath, "import test from 'node:test';\nimport {performance} from 'node:perf_hooks';\ntest('actual isolated preload sampling', () => {function profileCanaryWork(){const until=performance.now()+250; let n=0; while(performance.now()<until) n+=Math.sqrt(n+1); return n;} profileCanaryWork();});\n");
    const preload = pathToFileURL(join(here, 'worker.mjs')).href;
    const canaryOut = join(out, 'canary'); mkdirSync(canaryOut);
    const canary = await requireCommand('canary', [process.execPath, '--import=' + preload, '--test', '--test-reporter=tap', canaryPath],
        {env: {...baseEnv, WIRE_PROFILE_OUT: canaryOut, WIRE_PROFILE_TARGET: canaryPath}});
    report.canaryCoverage = captureCoverage(canaryOut, canary, canaryPath, [pathToFileURL(canaryPath).href]); save();
    assert.equal(report.canaryCoverage.verdict, 'PASS', 'actual worker preload/flush canary coverage failed');
    const workerOut = join(out, 'worker'); mkdirSync(workerOut);
    const wait = await command('retained-wire', [process.execPath, '--import=' + preload, '--test', '--test-reporter=tap', testPath],
        {env: {...baseEnv, WIRE_PROFILE_OUT: workerOut, WIRE_PROFILE_TARGET: testPath}});
    const raw = textOf('retained-wire') + readFileSync(join(out, 'retained-wire.stderr.log'), 'utf8');
    report.workload = workloadVerdict(wait, raw);
    report.coverage = captureCoverage(workerOut, wait, testPath,
        [pathToFileURL(join(product, 'build')).href + '/', pathToFileURL(join(product, 'node_modules/@eosrio/hyperion-sdk')).href + '/']);
    json('host-after.json', {at: new Date().toISOString(), freemem: os.freemem(), loadavg: os.loadavg(),
        cgroup: readOptional('/proc/self/cgroup'), cgroupMountinfo: readOptional('/proc/self/mountinfo'),
        cgroupFiles: Object.fromEntries(['cpu.max','cpu.stat','memory.max','memory.current','memory.events','io.stat']
            .map(file => [file, readOptional('/sys/fs/cgroup/' + file)]))});
    // A failed original workload is retained and does not prevent recording post-run source custody.
    await requireCommand('source-clean-after', ['git', 'diff', '--exit-code', 'HEAD', '--']);
    report.collection = {verdict: 'PASS', meaning: 'Collection reached normal command wait and parsed evidence; workload and coverage verdicts remain independent.'};
} catch (error) {
    report.collection = {verdict: 'FAIL', error: String(error.stack ?? error)};
} finally {
    report.finishedAt = new Date().toISOString(); save();
    const artifacts = inventory(out).filter(row => !row.file.endsWith('/SHA256SUMS'));
    writeFileSync(join(out, 'SHA256SUMS'), artifacts.map(row => `${row.sha256}  ${row.file.slice(out.length + 1)}`).join('\n') + '\n');
    console.log(JSON.stringify({workload: report.workload, coverage: report.coverage.verdict, collection: report.collection}, null, 2));
    process.exitCode = report.workload.verdict === 'PASS' && report.coverage.verdict === 'PASS' && report.collection.verdict === 'PASS' ? 0 : 1;
}
