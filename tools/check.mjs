import { chromium } from 'playwright-core';
import { execSync } from 'node:child_process';
const base='http://127.0.0.1:7777';
const token=execSync(`curl -sS ${base}/`,{encoding:'utf8'}).match(/token:\s*"([^"]+)"/)?.[1];
const b=await chromium.launch({channel:'chrome',args:['--no-sandbox']});
const p=await b.newPage({viewport:{width:1500,height:950}});
const errs=[]; p.on('pageerror',e=>errs.push('pageerror: '+e.message));
p.on('console',m=>{if(m.type()==='error')errs.push('console: '+m.text());});
await p.route('**/vendor/addon-webgl.js',r=>r.fulfill({status:200,contentType:'application/javascript',
  body:'window.WebglAddon={WebglAddon:function(){this.activate=function(){};this.dispose=function(){};}};'}));
await p.goto(`${base}/?token=${token}`,{waitUntil:'domcontentloaded'});
await p.waitForTimeout(2500);
console.log(JSON.stringify(await p.evaluate(()=>({
  prRows:document.querySelectorAll('.prrow').length,
  queueRows:document.getElementById('rvlist')?.childElementCount??null,
  railNodes:document.querySelectorAll('#rail *').length,
})),null,0));
// Does the selection inversion fire? core owns the state, app.js registered the
// effect, and neither imports the other's module.
console.log('selection:', JSON.stringify(await p.evaluate(async()=>{
  const core=await import('/js/core.js');
  const before=core.selected;
  core.setSelected('probe-id');
  const after=core.selected;
  core.setSelected(before);
  return {before,after,listenerRan:after==='probe-id'};
})));
console.log('errors:',errs.length?errs:'NONE');
await b.close();
