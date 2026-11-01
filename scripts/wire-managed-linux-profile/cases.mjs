// Fixed failed-job identities only. These records do not accept caller-supplied refs or runtimes.
export const cases = Object.freeze({
    'node24-21ef': Object.freeze({source: '21ef67d84ad94fb35a0c1bba600a42df5318c404',
        tree: '95a331dbe4ba2b9438db7afabef35f57c35fe032', node: 'v24.22.1', npm: '11.19.1',
        issue: 'https://github.com/eosrio/hyperion-tools/issues/83', failedRun: '41256269394', failedJob: '124699981577', report: 'report.json'}),
    'node26-404d': Object.freeze({source: '404d36f58aaaf83b5b25c8e5a62d38ed582f5d57',
        tree: 'df4e4527bc6a3a34c5f046b5f6f5f192c382b66e', node: 'v26.13.0', npm: '11.20.0',
        issue: 'https://github.com/eosrio/hyperion-tools/issues/86', failedRun: '41262118567', failedJob: '124717329561', report: 'report-node26-404d.json'}),
});
export function selectCase(id) {
    if (!Object.hasOwn(cases, id)) throw new Error('Unknown fixed profiling case');
    return cases[id];
}
