"""Independent literal/source-layout engineering fixtures. Imports no plugin codec.

Wire source6508c9e8: chain/abi_serializer.cpp, fc/slug_name.hpp and
state_history/serialization.hpp. No deployed-contract or node-built claim.
"""
import json
import pathlib
import struct

ROOT = pathlib.Path(__file__).resolve().parent
PROFILE = 'wire-sysio-6508c9e8-engineering-v1'

def varuint(value):
    result = bytearray()
    while True:
        result.append((value & 127) | (128 if value > 127 else 0))
        value >>= 7
        if not value:
            return bytes(result)

def blob(value):
    return varuint(len(value)) + value

def text(value):
    return blob(value.encode())

vectors = []
def add(name, typ, data, expected, abi='ship.abi.json'):
    vectors.append(dict(id=name, operation='decodeValue', profileId=PROFILE,
                        abiPath=abi, typeName=typ, hex=data.hex(), expected=expected,
                        oracle='source-derived-independent-python'))

code = bytes.fromhex('0000000000eab0c7')  # exact sysio name from captured block
payer = bytes(8)                         # raw zero sentinel, no name hashing
key = bytes(range(64))
value = struct.pack('<Q', 2**64-1)
for table_id in (0, 65535):
    prefix = b'\x00' + code + payer + struct.pack('<H', table_id)
    add(f'source-primary-kv-{table_id}', 'contract_row_kv', prefix + blob(key) + blob(value),
        ['contract_row_kv_v0', dict(code='sysio', payer='', table_id=table_id, key=key.hex(), value=value.hex())])
    secondary = bytes(reversed(range(64)))
    add(f'source-secondary-kv-{table_id}', 'contract_index_kv', prefix + blob(secondary) + blob(key),
        ['contract_index_kv_v0', dict(code='sysio', payer='', table_id=table_id, sec_key=secondary.hex(), pri_key=key.hex())])

for kind, enum in ((2, 'CHAIN_KIND_EVM'), (99, 99)):
    # slug literal ETH = 0000000042150000, from independent source slug vectors.
    data = bytes.fromhex('0000000042150000') + struct.pack('<iI', kind, 1)
    data += text('Ethereum') + text('source-derived engineering fixture')
    data += b'\x00\x01' + struct.pack('<QQ', 1735689600000, 1735689600001)
    data += text('0x1234') + text('0x5678')
    add(f'source-6508-chain-row-enum-{kind}', 'chain_row', data,
        dict(code='ETH', kind=enum, external_chain_id=1, name='Ethereum',
             description='source-derived engineering fixture', is_depot=0, active=1,
             registered_at_ms='1735689600000', activated_at_ms='1735689600001',
             outpost=dict(opp_addr='0x1234', opp_inbound_addr='0x5678')),
        'artifacts/6508c9e8/sysio.chains.abi')

for micros, expected in ((-1, '-000000.000001'), (-1000000, '-000001'), (-3600000000, '-010000')):
    add(f'source-negative-time-{abs(micros)}', 'time_point', struct.pack('<q', micros), expected)

output = dict(schemaVersion=1, producer=dict(kind='independent-source-derived-python',
    sourceRevision='6508c9e8f4044df8d5acb082a7af3be6a80cd9a0', nodeExecution=False,
    contractDeployment=False), vectors=vectors)
(ROOT / 'extra-vectors.json').write_text(json.dumps(output, indent=2) + '\n', encoding='utf8', newline='\n')
