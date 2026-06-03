// WebSocket get_token_holders scale test — streams ALL holders of a token over the cc32d9 WS API and
// measures throughput. The real stress test for the token_holders segment table at WAX scale
// (eosio.token/WAX has millions of holders). Run with bun (native WebSocket, no deps):
//   bun ws-scale-test.mjs <ws-url> <chain> <contract> <symbol>
const [url, chain, contract, symbol] = process.argv.slice(2);
const ws = new WebSocket(url);
let rows = 0, bytes = 0, first = 0;
const t0 = process.hrtime.bigint();

ws.onopen = () => {
  ws.send(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'get_token_holders',
    params: { reqid: 1, network: chain, contract, currency: symbol } }));
};
ws.onmessage = (ev) => {
  bytes += ev.data.length;
  const m = JSON.parse(ev.data).params;
  if (!m) return;
  if (m.data) { if (rows === 0) first = Number(process.hrtime.bigint() - t0) / 1e6; rows++; }
  if (m.end) {
    const ms = Number(process.hrtime.bigint() - t0) / 1e6;
    console.log(JSON.stringify({
      token: `${contract}/${symbol}`, holders: rows, end_status: m.status,
      first_row_ms: +first.toFixed(1), total_ms: +ms.toFixed(0),
      rows_per_s: Math.round(rows / (ms / 1000)), mb: +(bytes / 1e6).toFixed(1),
    }));
    ws.close(); process.exit(0);
  }
};
ws.onerror = (e) => { console.error('WS error:', e?.message ?? e); process.exit(1); };
setTimeout(() => { console.error('timeout', rows, 'rows'); process.exit(1); }, 120000);
