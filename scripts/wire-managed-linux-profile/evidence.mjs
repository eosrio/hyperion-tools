import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';

const canonicalUrl = url => url.startsWith('/') ? pathToFileURL(url, {windows: false}).href : url;

export function inspectProfile(profile, requiredPrefixes) {
    assert(Array.isArray(profile.nodes) && profile.nodes.length > 0, 'missing CPU nodes');
    assert(Array.isArray(profile.samples) && profile.samples.length > 0, 'missing CPU samples');
    assert(Array.isArray(profile.timeDeltas) && profile.timeDeltas.length === profile.samples.length, 'incomplete sample times');
    assert(Number.isFinite(profile.startTime) && profile.endTime > profile.startTime, 'invalid profile interval');
    const nodes = new Map(profile.nodes.map(row => [row.id, row]));
    assert.equal(nodes.size, profile.nodes.length, 'duplicate CPU node');
    for (const id of profile.samples) assert(nodes.has(id), 'unresolved sample');
    for (const delta of profile.timeDeltas) assert(Number.isFinite(delta) && delta >= 0, 'invalid sample delta');
    const times = new Map();
    profile.samples.forEach((id, index) => times.set(id, (times.get(id) ?? 0) + profile.timeDeltas[index]));
    // Require sampled stacks, not merely loaded script names. Walk ancestors for callers.
    const parent = new Map();
    for (const node of profile.nodes) for (const child of node.children ?? []) {
        assert(nodes.has(child), 'unresolved child'); assert(!parent.has(child), 'multiple CPU parents'); parent.set(child, node.id);
    }
    const sampledUrls = new Set();
    for (const id of times.keys()) {
        const seen = new Set();
        for (let cursor = id; cursor !== undefined; cursor = parent.get(cursor)) {
            assert(!seen.has(cursor), 'cyclic CPU stack'); seen.add(cursor);
            const url = nodes.get(cursor).callFrame?.url;
            if (url) sampledUrls.add(url);
        }
    }
    const matched = requiredPrefixes.map(prefix => ({prefix, urls: [...sampledUrls].filter(url => canonicalUrl(url).startsWith(prefix))}));
    return {durationUs: profile.endTime - profile.startTime, samples: profile.samples.length,
        matched, sampledUrls: [...sampledUrls].sort(), topSelfSamples: [...times].sort((a,b) => b[1] - a[1]).slice(0, 35)
            .map(([id, sampledUs]) => ({sampledUs, ...nodes.get(id).callFrame})),
        warnings: ['V8 sample timestamps are not additive stage wall times or exact exclusive CPU. Native stacks, allocations and I/O causality are not covered.']};
}

export function assessCoverage({identities, exits, states, profiles, requiredPrefixes, runnerPid, target}) {
    const failures = [], workers = identities.filter(row => row.active);
    if (workers.length !== 1) failures.push(`expected one actual test worker, observed ${workers.length}`);
    const worker = workers[0];
    if (worker && (worker.context !== 'child-v8' || worker.ppid !== runnerPid || worker.pid === runnerPid || worker.argv[1] !== target))
        failures.push('worker identity does not match captured runner and target');
    const completed = [];
    for (const row of profiles) {
        try {
            assert(!row.parseError, row.parseError);
            assert(worker && row.receipt.identity.pid === worker.pid && row.receipt.identity.ppid === runnerPid, 'wrong profile process');
            assert(row.receipt.completed === true && ['periodic', 'beforeExit'].includes(row.receipt.reason), 'incomplete CPU chunk receipt');
            assert(row.receipt.chunkEndedMs > row.receipt.chunkStartedMs, 'invalid chunk interval');
            completed.push({...row, analysis: inspectProfile(row.profile, requiredPrefixes)});
        } catch (error) {failures.push(`${row.file}: ${error.message}`);}
    }
    if (completed.length === 0) failures.push('missing complete actual-worker CPU profiles');
    const sequences = completed.map(row => row.receipt.sequence);
    if (new Set(sequences).size !== sequences.length || sequences.some((sequence, index) => sequence !== index))
        failures.push('CPU chunk sequences are duplicate, missing or unordered');
    for (const prefix of requiredPrefixes) if (!completed.some(row => row.analysis.matched.some(match => match.prefix === prefix && match.urls.length > 0)))
        failures.push(`missing sampled actual module stack: ${prefix}`);
    const exit = exits.find(row => row.identity.pid === worker?.pid), state = states.find(row => row.identity.pid === worker?.pid);
    let censoredInterval = null;
    if (!exit?.naturalExitObserved || exit.profiling || exit.stopping || exit.boundedLimitReached || exit.errors?.length || state?.errors?.length) {
        failures.push('worker lifetime coverage incomplete; final interval is censored or failed');
        censoredInterval = {fromWorkerElapsedMs: state?.chunkStarted ?? null, throughWorkerElapsedMs: exit?.elapsedMs ?? null,
            upperBoundKnown: Boolean(exit?.naturalExitObserved), reason: 'No successfully flushed final interval and natural-exit receipt'};
    }
    if (exit && exit.sequence !== completed.length) failures.push('completed CPU chunk count differs from worker exit receipt');
    return {verdict: failures.length ? 'FAIL' : 'PASS', failures, worker: worker ?? null, censoredInterval,
        completedChunks: completed.map(({profile, ...row}) => row),
        intervalGaps: completed.slice(1).map((row, i) => ({fromMs: completed[i].receipt.chunkEndedMs, throughMs: row.receipt.chunkStartedMs})),
        scope: 'Actual test worker V8 JavaScript samples, periodic chunks. Rotation gaps are explicit; native stacks and external services are not covered.'};
}

export function workloadVerdict(wait, text) {
    const timedOut = /test timed out after 180000ms/.test(text);
    const counters = Object.fromEntries(['tests','pass','fail','cancelled','skipped','todo'].map(key => {
        const matches = [...text.matchAll(new RegExp(`^# ${key} (\\d+)\\r?$`, 'gm'))];
        return [key, matches.length === 1 ? Number(matches[0][1]) : null];
    }));
    const assertionsCompleted = counters.tests === 2 && counters.pass === 2 && counters.fail === 0 && counters.cancelled === 0 && counters.skipped === 0 && counters.todo === 0;
    const complete = assertionsCompleted && !timedOut && wait.code === 0 && wait.signal === null && wait.closeObserved && wait.stdoutEnded && wait.stderrEnded && !wait.spawnError && !wait.truncated && !wait.stdoutError && !wait.stderrError;
    return {verdict: complete ? 'PASS' : 'FAIL', timedOut, originalDeadlineMs: 180000,
        code: wait.code, signal: wait.signal, counters, waitObserved: wait.closeObserved, streamsEnded: Boolean(wait.stdoutEnded && wait.stderrEnded),
        meaning: 'Result of unchanged test assertions and timeout, independent of successful profile collection.'};
}
