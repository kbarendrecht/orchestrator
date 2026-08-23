import { chromium } from 'playwright-core';
import { execSync } from 'node:child_process';
const base='http://127.0.0.1:7777';
const token=execSync(`curl -sS ${base}/`,{encoding:'utf8'}).match(/token:\s*"([^"]+)"/)?.[1];
const b=await chromium.launch({channel:'chrome',args:['--no-sandbox']});
const p=await b.newPage({viewport:{width:1600,height:1000}});
const errs=[]; p.on('pageerror',e=>errs.push('pageerror: '+e.message));
p.on('console',m=>{if(m.type()==='error')errs.push('console: '+m.text());});
await p.route('**/vendor/addon-webgl.js',r=>r.fulfill({status:200,contentType:'application/javascript',
  body:'window.WebglAddon={WebglAddon:function(){this.activate=function(){};this.dispose=function(){};}};'}));
await p.goto(`${base}/?token=${token}`,{waitUntil:'domcontentloaded'});
await p.waitForTimeout(2500);
console.log('render:', JSON.stringify(await p.evaluate(()=>({
  prRows:document.querySelectorAll('.prrow').length,
  queueRows:document.getElementById('rvlist')?.childElementCount??null,
  railNodes:document.querySelectorAll('#rail *').length}))));
console.log('drawer inversion:', JSON.stringify(await p.evaluate(async()=>{
  const core=await import('/js/core.js');
  const before=core.drawerCollapsed;
  core.setDrawerCollapsed(!before);
  const mid=core.drawerCollapsed;
  core.setDrawerCollapsed(before);
  return {before,toggled:mid!==before,restored:core.drawerCollapsed===before};
})));
console.log('errors:',errs.length?errs:'NONE');
await b.close();
