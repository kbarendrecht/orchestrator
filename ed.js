const WebSocket=require('ws'); const fs=require('fs'); const sleep=ms=>new Promise(r=>setTimeout(r,ms));
let id=0,ws,pending=new Map(); const errors=[];
const send=(m,p)=>new Promise(res=>{const i=++id;pending.set(i,{res});ws.send(JSON.stringify({id:i,method:m,params:p||{}}))});
async function ev(e){const r=await send('Runtime.evaluate',{expression:e,awaitPromise:true,returnByValue:true});
  if(r.exceptionDetails)errors.push('EVAL: '+(r.exceptionDetails.exception?.description||r.exceptionDetails.text));return r.result?.value;}
(async()=>{
  const t=await fetch('http://127.0.0.1:9222/json/list').then(r=>r.json());
  ws=new WebSocket(t.find(x=>x.type==='page').webSocketDebuggerUrl,{maxPayload:64*1024*1024});
  ws.on('message',m=>{const d=JSON.parse(m);
    if(d.id&&pending.has(d.id)){pending.get(d.id).res(d.result);pending.delete(d.id);return;}
    if(d.method==='Runtime.exceptionThrown')errors.push('EXCEPTION: '+(d.params.exceptionDetails.exception?.description||d.params.exceptionDetails.text));
    if(d.method==='Runtime.consoleAPICalled'&&d.params.type==='error')errors.push('CONSOLE: '+d.params.args.map(a=>a.value||a.description).join(' '));});
  await new Promise(r=>ws.on('open',r));
  await send('Runtime.enable'); await send('Page.enable');
  await send('Page.navigate',{url:'http://127.0.0.1:7777/'}); await sleep(3500);
  // Pin to main: the worktree is clean, so there would be nothing to edit.
  await ev(`selected=null; diffState.base='upstream'`);
  await ev(`(()=>{const b=[...document.querySelectorAll('.ws')[0].querySelectorAll('.sess,.railbtn')][0]; return currentWorkspaceId()})()`);
  await ev(`snap.workspaces.find(w=>w.is_main).id`);
  await ev(`window.__forceMain=true; currentWorkspaceId=()=>'main'`);
  await ev(`openDiff()`); await sleep(3500);
  const pick=await ev(`(()=>{const f=diffState.summary.files.find(f=>!f.binary&&f.eager); return f?f.path:null})()`);
  await ev(`loadFile(${JSON.stringify(pick)})`); await sleep(2500);
  console.log('file      :', pick);
  await ev(`openEditor()`); await sleep(2500);
  console.log('editing   :', await ev(`editState.on`));
  console.log('base pane :', await ev(`document.querySelector('.editbase') ? document.querySelector('.editbase').textContent.length : 0`), 'chars');
  console.log('textarea  :', await ev(`document.querySelector('.editarea') ? document.querySelector('.editarea').value.length : 0`), 'chars');
  console.log('save shown:', await ev(`!document.getElementById('ovsave').hidden`));
  const r=await send('Page.captureScreenshot',{format:'png'});
  fs.writeFileSync(process.env.S+'/editor.png',Buffer.from(r.data,'base64'));
  console.log('errors:', errors.length?errors.join('\n'):'(none)');
  process.exit(0);
})();
