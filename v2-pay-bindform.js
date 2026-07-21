const { spawn } = require('child_process');
const BIN = '/srv/forge/workspaces/mobee-jobe/target/release/mobee';
const srv = spawn(BIN, ['mcp'], { env: { ...process.env }, stdio: ['pipe','pipe','pipe'] });
let buf=''; const pending=new Map(); let idc=0;
srv.stdout.on('data', d => { buf+=d; let i; while((i=buf.indexOf('\n'))>=0){ const l=buf.slice(0,i); buf=buf.slice(i+1); if(!l.trim())continue; let m; try{m=JSON.parse(l)}catch{continue} if(m.id&&pending.has(m.id)){pending.get(m.id)(m);pending.delete(m.id)} }});
srv.stderr.on('data', d => process.stderr.write('[srv] '+d));
const rpc=(method,params,t=150000)=>{const id=++idc;return new Promise((res,rej)=>{const to=setTimeout(()=>{pending.delete(id);rej(new Error(method+' timeout'))},t);pending.set(id,m=>{clearTimeout(to);res(m)});srv.stdin.write(JSON.stringify({jsonrpc:'2.0',id,method,params})+'\n')})};
const tool=async(n,a={},t=150000)=>{const m=await rpc('tools/call',{name:n,arguments:a},t);const txt=m.result?.content?.[0]?.text||'';let p;try{p=JSON.parse(txt)}catch{p={_text:txt}}if(m.result?.isError)p._isError=true;return p};
(async()=>{
  await rpc('initialize',{protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'v2-pay-bind',version:'0'}},15000);
  srv.stdin.write(JSON.stringify({jsonrpc:'2.0',method:'notifications/initialized',params:{}})+'\n');
  console.log(JSON.stringify(await tool('authorize_pay',{
    job_id:'2a195bece5f66125a633b2cd8182e41ac86497f7a7b679e7271e2894c4bded62',
    amount_sats:5,
    delivery_integrity_hash:'5ce37eeb39308a47ab9a853f2d92d4895d3ae494'
  },150000),null,1).slice(0,1500));
  srv.stdin.end(); srv.kill(); process.exit(0);
})().catch(e=>{console.error('ERR '+e.message);process.exit(1)});
