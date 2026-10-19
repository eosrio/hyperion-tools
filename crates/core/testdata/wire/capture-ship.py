"""Read-only bounded SHiP capture on the pre-existing isolated Wire node.
Uses only the Python standard library and official clio; never a new plugin codec.
"""
import socket,base64,hashlib,os,json,struct,subprocess,pathlib,urllib.request,datetime
CLIO='/srv/hyperion-wire-testnet/20261019-node-setup/runtime/wire-sysio/bin/clio'
assert hashlib.sha256(pathlib.Path(CLIO).read_bytes()).hexdigest()=='a8298cc96d0265b384aebba178d255ff73917fcae0a0d4abf086f45016efb76a','Unselected fixture binary'
ROOT=pathlib.Path('/srv/hyperion-wire-testnet/20261019-node-setup/codec-oracle-issue2')
ROOT.mkdir(parents=True,exist_ok=True)
info=json.load(urllib.request.urlopen('http://127.0.0.1:18888/v1/chain/get_info',timeout=10))
assert info['server_full_version_string']=='v1.0.0-7dede884e0150d36fa788e85a119e10653ade8ca','Unselected fixture node revision'
s=socket.create_connection(('127.0.0.1',18080),timeout=10);s.settimeout(20)
key=base64.b64encode(os.urandom(16)).decode();s.sendall(('GET / HTTP/1.1\r\nHost: 127.0.0.1:18080\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: '+key+'\r\nSec-WebSocket-Version: 13\r\n\r\n').encode())
received=b''
while b'\r\n\r\n' not in received:
 received+=s.recv(4096)
 if len(received)>65536:raise ValueError('oversized handshake headers')
header,buffer=received.split(b'\r\n\r\n',1)
assert header.startswith(b'HTTP/1.1 101 '),header
accept=base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
assert b'sec-websocket-accept: '+accept.lower() in header.lower()
def read(n):
 global buffer
 while len(buffer)<n:
  b=s.recv(min(65536,n-len(buffer)))
  if not b:raise ValueError('unexpected EOF')
  buffer+=b
 a,buffer=buffer[:n],buffer[n:];return a
def frame():
 chunks=[];message_opcode=None;total=0
 while True:
  a,b=read(2);n=b&127
  if n==126:n=struct.unpack('>H',read(2))[0]
  if n==127:n=struct.unpack('>Q',read(8))[0]
  total+=n;assert total<=16*1024*1024,'capture size bound exceeded'
  assert not b&128,'server must not mask'
  payload=read(n);opcode=a&15
  if opcode==9:send(payload,10);continue
  if message_opcode is None:
   assert opcode in(1,2),'unexpected initial websocket opcode';message_opcode=opcode
  else:assert opcode==0,'unexpected websocket continuation opcode'
  chunks.append(payload)
  if a&128:return message_opcode,b''.join(chunks)
def send(payload,opcode=2):
 mask=os.urandom(4);n=len(payload);prefix=bytes([128|opcode])
 if n<126:prefix+=bytes([128|n])
 elif n<65536:prefix+=bytes([254])+struct.pack('>H',n)
 else:prefix+=bytes([255])+struct.pack('>Q',n)
 s.sendall(prefix+mask+bytes(v^mask[i%4] for i,v in enumerate(payload)))
op,handshake=frame();assert op==1
abi=json.loads(handshake);abi_path=ROOT/'captured-ship.abi';abi_path.write_bytes(handshake)
commands=[]
def clio(args):
 r=subprocess.run([CLIO,'convert']+args,capture_output=True,text=True,timeout=20)
 commands.append({'args':['convert']+args,'exitCode':r.returncode,'stdout':r.stdout,'stderr':r.stderr})
 if r.returncode or not r.stdout.strip() or 'ERROR: Unable to convert' in r.stderr:raise ValueError('official clio rejected captured frame')
 return json.loads(r.stdout)
def decode(typ,blob):return clio(['unpack_hex',blob.hex(),'--type',typ,'--abi-file',str(abi_path)])
def pack(typ,value,name):
 p=ROOT/('ship-input-'+name+'.json');p.write_text(json.dumps(value),encoding='utf8');return bytes.fromhex(clio(['pack_hex',str(p),'--type',typ,'--abi-file',str(abi_path)]))
status_request=pack('request',['get_status_request_v0',{}],'status');assert status_request==b'\x00'
send(status_request);_,status=frame();status_json=decode('result',status)
block_num=info['last_irreversible_block_num']; request_value=['get_blocks_request_v0',{'start_block_num':block_num,'end_block_num':block_num+1,'max_messages_in_flight':1,'have_positions':[],'irreversible_only':True,'fetch_block':True,'fetch_traces':True,'fetch_deltas':True}]
request=pack('request',request_value,'blocks');send(request);_,block_result=frame();result_json=decode('result',block_result)
ack=pack('request',['get_blocks_ack_request_v0',{'num_messages':1}],'ack');send(ack)
s.close()
vectors=[]
def add(id,typ,blob,expected):vectors.append({'id':id,'operation':'decodeValue','profileId':'wire-sysio-7dede884-engineering-v1','abiPath':'ship.abi.json','typeName':typ,'hex':blob.hex(),'expected':expected,'oracle':'captured-official-node-and-offline-clio'})
add('captured-ship-status-request','request',status_request,['get_status_request_v0',{}]);add('captured-ship-status','result',status,status_json);add('captured-ship-block-request','request',request,request_value);add('captured-ship-block-result','result',block_result,result_json);add('captured-ship-ack','request',ack,['get_blocks_ack_request_v0',{'num_messages':1}])
payload=result_json[1]
for field,typ in [('block','signed_block'),('traces','transaction_trace[]'),('deltas','table_delta[]')]:
 if payload[field] is not None:
  blob=bytes.fromhex(payload[field]);add('captured-ship-'+field,typ,blob,decode(typ,blob))
print(json.dumps({'schemaVersion':1,'createdAt':datetime.datetime.now(datetime.timezone.utc).isoformat(),'sourceRevision':'7dede884e0150d36fa788e85a119e10653ade8ca','chainInfo':info,'handshakeSha256':hashlib.sha256(handshake).hexdigest(),'handshakeUtf8':handshake.decode(),'clioSha256':hashlib.sha256(pathlib.Path(CLIO).read_bytes()).hexdigest(),'nodeEndpoint':'loopback-only isolated engineering node','blockNum':block_num,'requestHex':request.hex(),'ackHex':ack.hex(),'vectors':vectors,'commands':commands,'limits':{'blocks':1,'maxFrameBytes':16777216,'socketTimeoutSeconds':20},'contractDeployment':False}))
