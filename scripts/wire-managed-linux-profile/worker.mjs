// Diagnostic preload only. It neither imports nor rewrites Hyperion modules.
import {Session} from 'node:inspector';
import {appendFileSync, readFileSync, writeFileSync, renameSync} from 'node:fs';
import {join, resolve} from 'node:path';
import {performance} from 'node:perf_hooks';
import {isMainThread, threadId} from 'node:worker_threads';

const target = process.env.WIRE_PROFILE_TARGET;
const out = process.env.WIRE_PROFILE_OUT;
if (out && target && isMainThread) {
    const identity = {pid: process.pid, ppid: process.ppid, threadId, argv: process.argv,
        execArgv: process.execArgv, execPath: process.execPath, version: process.version,
        context: process.env.NODE_TEST_CONTEXT ?? null, startedAt: new Date().toISOString()};
    const active = identity.context === 'child-v8' && resolve(process.argv[1] ?? '') === resolve(target);
    const prefix = join(out, `process-${process.pid}`);
    const put = (suffix, value) => {
        const path = prefix + suffix, temp = path + '.tmp';
        writeFileSync(temp, JSON.stringify(value, null, 2) + '\n', {flag: 'w'});
        renameSync(temp, path);
    };
    put('.identity.json', {...identity, active});
    if (active) {
        const started = performance.now();
        const chunkMs = 30000, intervalUs = 1000, maxChunks = 7;
        const session = new Session();
        let sequence = 0, profiling = false, stopping = false, chunkStarted = null;
        let timer, sampler, boundedLimitReached = false, errors = [];
        const safeRead = path => {try {return readFileSync(path, 'utf8');} catch (error) {return {unavailable: error.code};}};
        const resource = event => appendFileSync(prefix + '.resources.ndjson', JSON.stringify({event,
            elapsedMs: performance.now() - started, pid: process.pid, cpuUsage: process.cpuUsage(),
            memory: process.memoryUsage(), resourceUsage: process.resourceUsage(),
            stat: safeRead('/proc/self/stat'), status: safeRead('/proc/self/status'), io: safeRead('/proc/self/io')}) + '\n');
        const state = () => put('.state.json', {identity, sequence, profiling, stopping, chunkStarted,
            elapsedMs: performance.now() - started, chunkMs, intervalUs, maxChunks, boundedLimitReached, errors});
        const fail = error => {errors.push(String(error?.stack ?? error)); state();};
        function start() {
            if (sequence >= maxChunks) {boundedLimitReached = true; clearInterval(sampler); state(); return;}
            session.post('Profiler.start', error => {
                if (error) return fail(error);
                profiling = true; chunkStarted = performance.now() - started; state();
                timer = setTimeout(() => stop('periodic', true), chunkMs).unref();
            });
        }
        function stop(reason, restart) {
            if (!profiling || stopping) return;
            stopping = true; clearTimeout(timer); state();
            session.post('Profiler.stop', (error, result) => {
                if (error) {stopping = false; return fail(error);}
                const ended = performance.now() - started;
                const path = `.chunk-${String(sequence).padStart(2, '0')}.cpuprofile`;
                try {
                    put(path, result.profile);
                    put(path + '.receipt.json', {identity, sequence, reason, chunkStartedMs: chunkStarted,
                        chunkEndedMs: ended, intervalUs, completed: true});
                    sequence++; profiling = false; stopping = false; state(); resource('chunk-completed');
                    if (restart) start();
                    else session.disconnect();
                } catch (writeError) {fail(writeError);}
            });
        }
        resource('start');
        session.connect();
        session.post('Profiler.enable', error => {
            if (error) return fail(error);
            session.post('Profiler.setSamplingInterval', {interval: intervalUs}, samplingError => {
                if (samplingError) return fail(samplingError);
                start();
            });
        });
        sampler = setInterval(() => {try {resource('sample');} catch (error) {fail(error);}}, 1000).unref();
        process.once('beforeExit', () => {clearInterval(sampler); stop('beforeExit', false);});
        process.once('exit', code => {
            clearInterval(sampler); clearTimeout(timer);
            try {
                resource('exit');
                put('.exit.json', {identity, code, elapsedMs: performance.now() - started, sequence,
                    profiling, stopping, boundedLimitReached, errors, naturalExitObserved: true});
            } catch { /* Missing final receipt is a coverage failure in the collector. */ }
        });
    }
}
