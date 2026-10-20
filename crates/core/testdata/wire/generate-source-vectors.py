"""Regenerate source-derived 6508 slug cases without importing the codec under test.
The packing expression is from the pinned fc::slug_name_traits/basic_name headers.
These are source-derived expectations, not a node or clio oracle for 6508.
"""
import json,pathlib,struct,hashlib
root=pathlib.Path(__file__).parent
alphabet='\0ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_'
def slug(s):
 assert len(s)<=8 and (not s or s[0]in alphabet[1:27])
 return sum(alphabet.index(c)<<(42-i*6) for i,c in enumerate(s))
def text(s):
 b=s.encode('utf8');assert len(b)<128;return bytes([len(b)])+b
profile='wire-sysio-6508c9e8-engineering-v1'
source='https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/libfc/include/fc/slug_name.hpp'
vectors=[]
for name in ['','ETH','USDC','WIRE_123']:
 vectors.append({'id':'6508-source-slug-'+(name or 'zero'),'operation':'decodeValue','profileId':profile,'abiPath':'artifacts/6508c9e8/sysio.chains.abi','typeName':'slug_name','hex':struct.pack('<Q',slug(name)).hex(),'expected':name,'oracle':'source-derived-independent-python','source':source})
value={'code':'ETH','kind':'CHAIN_KIND_EVM','external_chain_id':1,'name':'Ethereum','description':'source fixture','is_depot':0,'active':1,'registered_at_ms':'1735689600000','activated_at_ms':'1735689600500','outpost':{'opp_addr':'0x1234','opp_inbound_addr':'0x5678'}}
b=struct.pack('<QiI',slug('ETH'),2,1)+text(value['name'])+text(value['description'])+b'\x00\x01'+struct.pack('<QQ',1735689600000,1735689600500)+text('0x1234')+text('0x5678')
vectors.append({'id':'6508-source-chain-row','operation':'decodeValue','profileId':profile,'abiPath':'artifacts/6508c9e8/sysio.chains.abi','typeName':'chain_row','hex':b.hex(),'expected':value,'oracle':'source-derived-independent-python','source':source})
(root/'source-vectors.json').write_text(json.dumps({'schemaVersion':1,'producer':{'kind':'independent-source-derived-python','sourceRevision':'6508c9e8f4044df8d5acb082a7af3be6a80cd9a0','nodeExecution':False},'vectors':vectors},indent=2)+'\n',encoding='utf8')
print('wrote',len(vectors),'explicitly source-derived slug cases')
