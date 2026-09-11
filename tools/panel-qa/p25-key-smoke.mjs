// Real embedded page + real API, isolated config and deliberately absent radio.
// cargo build -p usdr-server --bin usdr
// node tools/panel-qa/p25-key-smoke.mjs
import {spawn} from 'node:child_process';
import {createServer} from 'node:net';
import {mkdtempSync,readFileSync,writeFileSync,rmSync,mkdirSync,statSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
const scratch=mkdtempSync(join(tmpdir(),'usdr-p25-web-'));
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function freePort(){const s=createServer();await new Promise(r=>s.listen(0,'127.0.0.1',r));const p=s.address().port;await new Promise(r=>s.close(r));return p;}
const port=await freePort(), cdpPort=await freePort(), base=`http://127.0.0.1:${port}`;
const config=join(scratch,'usdr.toml');
writeFileSync(config,'serial="usdr-p25-test-no-radio"\nfreq_hz=851012500.0\ninspect_hz=851012500.0\nmode="p25"\n');
let server,chrome,ws;const errors=[];
function check(name,ok){if(!ok)throw new Error(name);console.log('PASS '+name);}
async function start(){
 server=spawn(resolve('target/debug/usdr'),['--bind',`127.0.0.1:${port}`,'--config',config],{stdio:'ignore'});
 for(let i=0;i<60;i++){try{if((await fetch(base+'/api/sdr/status')).ok)return;}catch{}await sleep(200);}
 throw new Error('server startup timed out');
}
async function stop(){const child=server;if(!child)return;const done=new Promise(r=>child.once('exit',r));child.kill('SIGTERM');await Promise.race([done,sleep(1500)]);if(child.exitCode===null)child.kill('SIGKILL');server=null;}
try{
 await start();
 chrome=spawn('google-chrome',['--headless=new','--disable-gpu','--no-sandbox',`--user-data-dir=${join(scratch,'chrome')}`,`--remote-debugging-port=${cdpPort}`,'--window-size=1280,1000',base],{stdio:'ignore'});
 let target;
 for(let i=0;i<80;i++){try{target=(await(await fetch(`http://127.0.0.1:${cdpPort}/json/list`)).json()).find(t=>t.type==='page');if(target)break;}catch{}await sleep(200);}
 ws=new WebSocket(target.webSocketDebuggerUrl);await new Promise((r,j)=>{ws.onopen=r;ws.onerror=j;});
 let id=0;const pending=new Map();
 ws.onmessage=e=>{const m=JSON.parse(e.data);if(m.method==='Runtime.exceptionThrown')errors.push(m.params.exceptionDetails.text);if(m.id){pending.get(m.id)?.(m);pending.delete(m.id);}};
 const send=(method,params={})=>new Promise(r=>{const n=++id;pending.set(n,r);ws.send(JSON.stringify({id:n,method,params}));});
 const js=async expression=>{const r=await send('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(r.result?.exceptionDetails)throw new Error(r.result.exceptionDetails.text);return r.result?.result?.value;};
 const until=async expression=>{for(let i=0;i<80;i++){if(await js(expression))return;await sleep(100);}throw new Error('Timed out: '+expression+' state: '+JSON.stringify(await js("({mode:sdrMode,frequency:sdrInspectHz,status:document.getElementById('p25KeyStatus').textContent,err:document.getElementById('err').textContent})"))); };
 await send('Runtime.enable');
 const park=async()=>{
  await until("typeof sdrStatus !== 'undefined' && sdrStatus !== null");
  // No hardware is opened: inject only the radio's acknowledged tune/mode.
  // The embedded form, all key API calls, persistence and restart are real.
  await js("window.p25TestHz=851012500; window.originalUpdateSdrStatus=updateSdrStatus; updateSdrStatus=s=>window.originalUpdateSdrStatus({...s,mode:'p25',inspect_hz:window.p25TestHz}); updateSdrStatus(sdrStatus)");
 };
 await park();
 await until("document.getElementById('p25KeySave') && !document.getElementById('p25KeySave').disabled");
 check('P25 channel configuration is visible',await js("!document.getElementById('p25KeyPanel').hidden"));
 await js("document.querySelector('#p25KeyPanel summary').click();document.getElementById('p25KeyPanel').scrollIntoView({block:'center'})");
 check('key is a masked password field',await js("document.getElementById('p25KeyValue').type === 'password'"));
 const fill=async(type,key)=>js(`document.getElementById('p25KeyType').value='${type}';document.getElementById('p25KeyType').dispatchEvent(new Event('change'));document.getElementById('p25KeyId').value='1234';document.getElementById('p25KeyValue').value='${key}';document.getElementById('p25KeyForm').requestSubmit();`);
 await fill(170,'123');
 check('invalid key gets an inline error',await js("document.getElementById('p25KeyStatus').dataset.error === 'true'"));
 await fill(170,'0001020304');
 await until("document.getElementById('p25KeyStatus').textContent.startsWith('Key saved.')");
 check('save clears the secret input',await js("document.getElementById('p25KeyValue').value === ''"));
 let response=await fetch(base+'/api/sdr/p25/key?frequency_hz=851012500');let status=await response.json();
 check('status is write-only and not cached',status.configured&&status.algorithm===170&&!('key'in status)&&response.headers.get('cache-control')==='no-store');
 const file=config.replace(/\.toml$/,'.p25-keys.json');
 check('saved key file has mode 0600',(statSync(file).mode&0o777)===0o600);
 check('key excluded from ordinary settings',!readFileSync(config,'utf8').includes('0001020304'));
 check('key excluded from browser storage',await js("!JSON.stringify(localStorage).includes('0001020304') && !JSON.stringify(sessionStorage).includes('0001020304')"));
 await fill(132,'01'.repeat(32));
 await until("document.getElementById('p25KeyStatus').textContent.startsWith('Key saved.') && !document.getElementById('p25KeySave').disabled");
 status=await(await fetch(base+'/api/sdr/p25/key?frequency_hz=851012500')).json();
 check('key type can be replaced live',status.algorithm===132);
 const rejected=await fetch(base+'/api/sdr/p25/key',{method:'POST',headers:{Origin:'https://unrelated.example','Content-Type':'application/json'},body:JSON.stringify({frequency_hz:851012500,action:'remove'})});
 check('cross-origin key changes rejected',rejected.status===400);
 // Reload the same real page after a real process restart.
 await stop();await start();await send('Page.reload');await sleep(300);await park();
 await until("document.getElementById('p25KeySave') && !document.getElementById('p25KeySave').disabled && document.getElementById('p25KeyType').value === '132'");
 check('restart restores metadata but never secret value',await js("document.getElementById('p25KeyValue').value === '' && document.getElementById('p25KeyId').value === '1234'"));
 await js("document.getElementById('p25KeyPanel').open=true;document.getElementById('p25KeyPanel').scrollIntoView({block:'center'})");
 mkdirSync('.hermes/qa',{recursive:true});
 for(const width of [1280,640]){
  await send('Emulation.setDeviceMetricsOverride',{width,height:900,deviceScaleFactor:1,mobile:false});
  await js("document.getElementById('p25KeyPanel').scrollIntoView({block:'center'})");await sleep(150);
  const shot=await send('Page.captureScreenshot',{format:'png'});writeFileSync(`.hermes/qa/p25-key-${width}.png`,Buffer.from(shot.result.data,'base64'));
  check(`key form fits at ${width}px`,await js("document.getElementById('p25KeyForm').scrollWidth <= document.getElementById('p25KeyForm').clientWidth"));
 }
 // A simulated acknowledged channel change tests the UI without a real dongle.
 await js("window.p25TestHz=851025000;updateSdrStatus(sdrStatus)");
 await until("!document.getElementById('p25KeySave').disabled && document.getElementById('p25KeyFrequency').textContent.includes('851.025000')");
 check('different frequency has no saved key',await js("document.getElementById('p25KeyRemove').disabled && document.getElementById('p25KeyId').value === ''"));
 await js("window.p25TestHz=851012500;updateSdrStatus(sdrStatus)");
 await until("!document.getElementById('p25KeyRemove').disabled");
 await js("document.getElementById('p25KeyRemove').click()");
 await until("document.getElementById('p25KeyStatus').textContent.startsWith('Channel key removed.')");
 status=await(await fetch(base+'/api/sdr/p25/key?frequency_hz=851012500')).json();
 check('remove clears persisted channel key',!status.configured&&readFileSync(file,'utf8')==='{}');
 check('no JavaScript exceptions',errors.length===0);
 console.log('Screenshots: .hermes/qa/p25-key-1280.png and p25-key-640.png');
}finally{ws?.close();chrome?.kill();await stop();await sleep(200);rmSync(scratch,{recursive:true,force:true});}
