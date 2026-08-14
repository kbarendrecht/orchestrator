const WebSocket=require('ws'); const sleep=ms=>new Promise(r=>setTimeout(r,ms));
let id=0,ws,pending=new Map();
const send=(m,p)=>new Promise(res=>{const i=++id;pending.set(i,{res});ws.send(JSON.stringify({id:i,method:m,params:p||{}}))});
async function ev(e){const r=await send('Runtime.evaluate',{expression:e,awaitPromise:true,returnByValue:true});
  if(r.exceptionDetails)return 'THREW: '+(r.exceptionDetails.exception?.description||r.exceptionDetails.text);return r.result?.value;}
(async()=>{
  const t=await fetch('http://127.0.0.1:9222/json/list').then(r=>r.json());
  ws=new WebSocket(t.find(x=>x.type==='page').webSocketDebuggerUrl,{maxPayload:64*1024*1024});
  ws.on('message',m=>{const d=JSON.parse(m); if(d.id&&pending.has(d.id)){pending.get(d.id).res(d.result);pending.delete(d.id);}});
  await new Promise(r=>ws.on('open',r));
  await send('Runtime.enable');
  console.log('workspaces  :', await ev(`JSON.stringify(snap.workspaces.map(w=>w.id))`));
  console.log('currentWs   :', await ev(`String(currentWorkspaceId())`));
  console.log('diffState   :', await ev(`JSON.stringify({base:diffState.base,open:diffState.open,summary:diffState.summary===null?'null':typeof diffState.summary})`));
  console.log('direct get  :', await ev(`get('/api/diff?workspace=main&base=upstream').then(d=>'files='+d.files.length).catch(e=>'ERR: '+e.message)`));
  console.log('loadSummary :', await ev(`loadSummary().then(()=>diffState.summary?('files='+diffState.summary.files.length):'still null')`));
  process.exit(0);
})();
